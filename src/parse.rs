use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

// ── Public types ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub sessions: Vec<Session>,
    pub total_tokens: u64,
    pub last_active: Option<DateTime<Utc>>,
    // Pre-aggregated row totals. Computed once in `load_all_projects`
    // (sessions are immutable thereafter) so the TUI can render the
    // projects list in O(1) per row instead of summing every session
    // every redraw.
    pub total_msgs: u64,
    pub total_dur_ms: u64,
    pub total_cost: f64,
}

// Two storage tiers backing this struct:
//   - `.bin`  — header (id, summary, first_user_msg, started_at,
//               turn_count, model, msg_count): everything the projects
//               list view needs. Tiny per session.
//   - `.msgs` — the `messages` Vec only. Loaded on demand when the user
//               opens a session (TUI) or when web `generate` walks for
//               per-session JSON output.
//
// Token totals + per-column costs + ended_at are owned by the canonical
// aggregation cache (`src/cache/`); `load_all_projects` populates them
// after load via `cache::per_session_totals`. They live in this struct
// only as runtime fields so downstream renderers can read them off
// `&Session` without threading the cache through every call site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub summary: Option<String>,
    pub first_user_msg: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub turn_count: usize,
    pub model: Option<String>,
    /// Number of messages in the `.msgs` blob. Persisted so the
    /// projects-list views can render counts without touching the blob.
    pub msg_count: u32,
    /// Working directory recorded in the JSONL (`cwd` field on user/assistant
    /// lines). Authoritative source for the project's real filesystem path —
    /// the parent directory's dash-encoded name (`-Users-me-code-foo-bar`)
    /// is ambiguous when the real path contains hyphens.
    #[serde(default)]
    pub cwd: Option<String>,

    // ── Lazy / runtime-populated fields below: not in `.bin` ──
    /// JSONL file this session was loaded from. Set by
    /// `load_all_projects` and used by `ensure_messages_loaded` to find
    /// the matching `.msgs` blob.
    #[serde(skip)]
    pub file_path: PathBuf,
    /// Empty after a header-only load. Call `ensure_messages_loaded` (or
    /// reparse) before iterating content.
    #[serde(skip)]
    pub messages: Vec<Message>,
    #[serde(skip)]
    pub total_input_tokens: u64,
    #[serde(skip)]
    pub total_output_tokens: u64,
    #[serde(skip)]
    pub total_cache_read: u64,
    #[serde(skip)]
    pub total_cache_create: u64,
    #[serde(skip)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub cost: f64,
    #[serde(skip)]
    pub cost_input: f64,
    #[serde(skip)]
    pub cost_output: f64,
    #[serde(skip)]
    pub cost_cache_read: f64,
    #[serde(skip)]
    pub cost_cache_create: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub timestamp: Option<DateTime<Utc>>,
    pub kind: MessageKind,
    pub content: String,
    pub tokens: Option<TokenUsage>,
    pub tool_name: Option<String>,
    pub model: Option<String>,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageKind {
    User,
    Assistant,
    ToolUse,
    ToolResult,
    Thinking,
    System,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
}

// ── Per-session binary cache (postcard) ──
//
// Three files per session under ~/.claude/ccaudit-cache/<hash>.*:
//   .meta — fingerprint (version + JSONL mtime + JSONL size)
//   .bin  — Session header (no messages, no totals)
//   .msgs — Vec<Message>, loaded only when content is needed
//
// The split exists because cold TUI startup (and `web --no-serve`'s
// projects-list render) only needs the header; reading the messages
// blob for every session just to call `.messages.len()` was the hot
// cost on warm runs.

fn cache_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("ccaudit-cache"))
}

fn file_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Some((mtime, meta.len()))
}

fn cache_key(path: &Path) -> String {
    format!("{:016x}", crate::source::path_hash(path))
}

// Bumped whenever Session header or Message struct changes shape, OR
// when the on-disk encoding changes.
const CACHE_VERSION: u8 = 0;

#[derive(Serialize, Deserialize)]
struct CacheMeta {
    version: u8,
    mtime_secs: u64,
    size: u64,
}

/// Header-only load: reads `.meta` for invalidation, then `.bin`.
/// Messages stay empty; call `load_messages_into` to fill them.
fn try_load_cached_header(path: &Path) -> Option<Session> {
    let dir = cache_dir()?;
    let key = cache_key(path);
    let (cur_mtime, cur_size) = file_fingerprint(path)?;
    let meta_bytes = fs::read(dir.join(format!("{key}.meta"))).ok()?;
    let meta: CacheMeta = postcard::from_bytes(&meta_bytes).ok()?;
    if meta.version != CACHE_VERSION || meta.mtime_secs != cur_mtime || meta.size != cur_size {
        return None;
    }
    let data = fs::read(dir.join(format!("{key}.bin"))).ok()?;
    postcard::from_bytes(&data).ok()
}

/// Read both header and messages from the per-session cache.
///
/// Both come from the same fingerprint slot, so this returns `None` on
/// any miss or mismatch. Used by providers (see
/// `ClaudeCode::parse_session`) to skip the JSONL reparse when the
/// cache is already fresh.
pub fn try_load_cached_full(path: &Path) -> Option<Session> {
    let dir = cache_dir()?;
    let key = cache_key(path);
    let (cur_mtime, cur_size) = file_fingerprint(path)?;
    let meta_bytes = fs::read(dir.join(format!("{key}.meta"))).ok()?;
    let meta: CacheMeta = postcard::from_bytes(&meta_bytes).ok()?;
    if meta.version != CACHE_VERSION || meta.mtime_secs != cur_mtime || meta.size != cur_size {
        return None;
    }
    let header_bytes = fs::read(dir.join(format!("{key}.bin"))).ok()?;
    let mut session: Session = postcard::from_bytes(&header_bytes).ok()?;
    let msgs_bytes = fs::read(dir.join(format!("{key}.msgs"))).ok()?;
    session.messages = postcard::from_bytes(&msgs_bytes).ok()?;
    Some(session)
}

/// Save a freshly-parsed session to the per-session cache.
///
/// Public so the source-trait implementations can persist what they
/// just parsed, letting subsequent `load_all_projects` cache lookups
/// skip the work.
pub fn save_session_to_cache(path: &Path, session: &Session) {
    save_to_cache(path, session);
}

/// Lazy-load the messages blob for `path`. Returns None if the cache
/// is missing or stale; caller falls back to `parse_session`.
pub fn load_messages_for(path: &Path) -> Option<Vec<Message>> {
    let dir = cache_dir()?;
    let key = cache_key(path);
    let (cur_mtime, cur_size) = file_fingerprint(path)?;
    let meta_bytes = fs::read(dir.join(format!("{key}.meta"))).ok()?;
    let meta: CacheMeta = postcard::from_bytes(&meta_bytes).ok()?;
    if meta.version != CACHE_VERSION || meta.mtime_secs != cur_mtime || meta.size != cur_size {
        return None;
    }
    let data = fs::read(dir.join(format!("{key}.msgs"))).ok()?;
    postcard::from_bytes(&data).ok()
}

/// Convenience: load messages for `path` into `session.messages` if
/// they're not already present. No-op if the cache is missing — callers
/// that need a guarantee should re-parse the JSONL on `false`.
pub fn ensure_messages_loaded(session: &mut Session, path: &Path) -> bool {
    if !session.messages.is_empty() {
        return true;
    }
    if let Some(msgs) = load_messages_for(path) {
        session.messages = msgs;
        return true;
    }
    if let Some(s) = parse_session(path) {
        session.messages = s.messages;
        return true;
    }
    false
}

fn save_to_cache(path: &Path, session: &Session) {
    let Some(dir) = cache_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let key = cache_key(path);
    let Some(fp) = file_fingerprint(path) else {
        return;
    };
    // Write header + messages first, then meta last — meta is the
    // gate readers check, so a half-written cache reads as stale.
    if let Ok(data) = postcard::to_allocvec(session) {
        let _ = fs::write(dir.join(format!("{key}.bin")), data);
    }
    if let Ok(data) = postcard::to_allocvec(&session.messages) {
        let _ = fs::write(dir.join(format!("{key}.msgs")), data);
    }
    let meta = CacheMeta {
        version: CACHE_VERSION,
        mtime_secs: fp.0,
        size: fp.1,
    };
    if let Ok(meta_bytes) = postcard::to_allocvec(&meta) {
        let _ = fs::write(dir.join(format!("{key}.meta")), meta_bytes);
    }
}

// JSON parser switch. Default uses serde_json on the immutable mmap'd
// slice; with `--features simd-json`, the slice is copied into a mutable
// buffer that simd-json's SIMD-accelerated parser scribbles in place.
// The copy is cheap relative to the parse cost on lines >100 bytes.
#[cfg(not(feature = "simd-json"))]
fn json_from_slice<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Option<T> {
    serde_json::from_slice(line).ok()
}

#[cfg(feature = "simd-json")]
fn json_from_slice<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Option<T> {
    let mut buf = line.to_vec();
    simd_json::serde::from_slice(&mut buf).ok()
}

// ── JSONL deserialization types ──

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    subtype: Option<String>,
    timestamp: Option<String>,
    message: Option<RawMessage>,
    #[serde(rename = "durationMs")]
    duration_ms: Option<u64>,
    // Claude Code emits the launch-time `cwd` on every user/assistant line.
    // We grab the first non-empty one to recover the unambiguous filesystem
    // path (the parent dir's dash-encoded name loses real hyphens).
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct RawMessage {
    id: Option<String>,
    // Claude Code's `content` is either a string (early user messages)
    // or an array of typed blocks (`text` / `thinking` / `tool_use` /
    // `tool_result`). Modeling that as `RawContent` lets serde do all
    // the field plucking up-front instead of walking a Value at runtime.
    content: Option<RawContent>,
    model: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawContent {
    Text(String),
    Blocks(Vec<RawBlock>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        name: String,
        // Typed projection of the input fields we actually render. Anything
        // outside this set is skipped at deserialize time, so we never
        // allocate a `serde_json::Value` tree for the input.
        #[serde(default)]
        input: ToolInput,
    },
    ToolResult {
        // Some content arrays nest content as a string; others as an
        // array of `{type:"text", text:"..."}`. Capture either.
        content: Option<RawContent>,
    },
    // Anything we don't model (image blocks today, future kinds) is
    // dropped silently — same behavior as the previous `_ => {}` arm.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ToolInput {
    command: Option<String>,
    description: Option<String>,
    file_path: Option<String>,
    pattern: Option<String>,
}

// ── Parsing helpers ──

#[allow(clippy::indexing_slicing)] // indices are bounds-checked by b.len() >= 20
fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    // Fast path for "2026-03-30T14:10:41.157Z" format (fixed layout)
    let b = s.as_bytes();
    if b.len() >= 20 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T' {
        let year = i32::try_from(fast_parse_u32(&b[0..4])?).ok()?;
        let month = fast_parse_u32(&b[5..7])?;
        let day = fast_parse_u32(&b[8..10])?;
        let hour = fast_parse_u32(&b[11..13])?;
        let min = fast_parse_u32(&b[14..16])?;
        let sec = fast_parse_u32(&b[17..19])?;
        let ndt = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, min, sec)?;
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc));
    }
    // Fallback
    s.parse::<DateTime<Utc>>().ok()
}

fn fast_parse_u32(b: &[u8]) -> Option<u32> {
    let mut n = 0u32;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n * 10 + u32::from(c - b'0');
    }
    Some(n)
}

// Take ownership so the short-string path is a move, not a clone. On the
// truncation path, reuse the buffer (truncate + push_str) instead of
// allocating a new String through `format!`.
fn truncate_str(mut s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("...");
    s
}

fn extract_text_content(content: RawContent) -> Vec<(MessageKind, String, Option<String>)> {
    match content {
        RawContent::Text(s) => vec![(MessageKind::User, s, None)],
        RawContent::Blocks(blocks) => {
            let mut out = Vec::with_capacity(blocks.len());
            for b in blocks {
                match b {
                    RawBlock::Text { text } if !text.is_empty() => {
                        out.push((MessageKind::Assistant, text, None));
                    }
                    RawBlock::Thinking { thinking } if !thinking.is_empty() => {
                        out.push((MessageKind::Thinking, thinking, None));
                    }
                    RawBlock::ToolUse { name, input } => {
                        let input_str = format_tool_input(&name, &input);
                        out.push((MessageKind::ToolUse, input_str, Some(name)));
                    }
                    RawBlock::ToolResult {
                        content: Some(RawContent::Text(text)),
                    } if !text.is_empty() => {
                        out.push((MessageKind::ToolResult, truncate_str(text, 500), None));
                    }
                    RawBlock::ToolResult {
                        content: Some(RawContent::Blocks(inner)),
                    } => {
                        // Tool results occasionally nest `[{type:"text", text:"..."}]`
                        // — pull the first text block, ignore the rest (matches the
                        // pre-typed-deserialize behavior).
                        for ib in inner {
                            if let RawBlock::Text { text } = ib {
                                if !text.is_empty() {
                                    out.push((
                                        MessageKind::ToolResult,
                                        truncate_str(text, 500),
                                        None,
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            out
        }
    }
}

fn format_tool_input(tool: &str, input: &ToolInput) -> String {
    let cmd = input.command.as_deref().unwrap_or("");
    let path = input.file_path.as_deref().unwrap_or("");
    let pat = input.pattern.as_deref().unwrap_or("");
    let desc = input.description.as_deref();
    match tool {
        "Bash" => match desc {
            Some(d) => format!("$ {cmd}\n  # {d}"),
            None => format!("$ {cmd}"),
        },
        "Read" => format!("read {path}"),
        "Write" => format!("write {path}"),
        "Edit" => format!("edit {path}"),
        "Glob" => format!("glob {pat}"),
        "Grep" => format!("grep {pat}"),
        "Agent" => format!("agent: {}", desc.unwrap_or("agent")),
        // Unknown tools: show name only. The previous fallback re-serialized
        // the input as JSON, which required keeping a full `serde_json::Value`
        // around — pricey when 1/3 of all messages are tool_use lines.
        _ => String::new(),
    }
}

// ── Core parser ──

// Parsed data from a single JSONL line, used for parallel intra-file parsing
struct ParsedLine {
    kind: LineParsed,
    timestamp: Option<DateTime<Utc>>,
    cwd: Option<String>,
}

enum LineParsed {
    User {
        parts: Vec<(MessageKind, String, Option<String>)>,
    },
    Assistant {
        parts: Vec<(MessageKind, String, Option<String>)>,
        model: Option<String>,
        tokens: Option<TokenUsage>,
        message_id: Option<String>,
    },
    Summary(String),
    System {
        duration_ms: u64,
    },
}

#[allow(clippy::indexing_slicing)]
fn parse_one_line(line: &[u8]) -> Option<ParsedLine> {
    // Quick reject before paying for full JSON parse. Each pattern is the
    // full top-level type field, so nested `"type":"text"` blocks inside
    // assistant content can't false-match. Finders are precompiled once
    // per process so the per-line cost is just the SIMD scan itself.
    use std::sync::OnceLock;
    static FINDERS: OnceLock<[memchr::memmem::Finder<'static>; 4]> = OnceLock::new();
    let finders = FINDERS.get_or_init(|| {
        [
            memchr::memmem::Finder::new(b"\"type\":\"user\""),
            memchr::memmem::Finder::new(b"\"type\":\"assistant\""),
            memchr::memmem::Finder::new(b"\"type\":\"summary\""),
            memchr::memmem::Finder::new(b"\"type\":\"system\""),
        ]
    });
    if !finders.iter().any(|f| f.find(line).is_some()) {
        return None;
    }

    let raw: RawLine = json_from_slice(line)?;
    let ts = raw.timestamp.as_deref().and_then(parse_timestamp);
    let msg_type = raw.msg_type.as_deref().unwrap_or("");
    let cwd = raw.cwd.filter(|s| !s.is_empty());

    match msg_type {
        "user" => {
            let msg = raw.message?;
            let content = msg.content?;
            let parts = extract_text_content(content);
            Some(ParsedLine {
                kind: LineParsed::User { parts },
                timestamp: ts,
                cwd,
            })
        }
        "assistant" => {
            let msg = raw.message?;
            let content = msg.content?;
            let parts = extract_text_content(content);
            let tokens = msg.usage.as_ref().map(|u| TokenUsage {
                input: u.input_tokens.unwrap_or(0),
                output: u.output_tokens.unwrap_or(0),
                cache_read: u.cache_read_input_tokens.unwrap_or(0),
                cache_create: u.cache_creation_input_tokens.unwrap_or(0),
            });
            Some(ParsedLine {
                kind: LineParsed::Assistant {
                    parts,
                    model: msg.model,
                    tokens,
                    message_id: msg.id,
                },
                timestamp: ts,
                cwd,
            })
        }
        "summary" => {
            let msg = raw.message?;
            let text = match msg.content? {
                RawContent::Text(s) => s,
                RawContent::Blocks(blocks) => blocks.into_iter().find_map(|b| match b {
                    RawBlock::Text { text } => Some(text),
                    _ => None,
                })?,
            };
            Some(ParsedLine {
                kind: LineParsed::Summary(text),
                timestamp: ts,
                cwd,
            })
        }
        "system" => {
            if raw.subtype.as_deref() == Some("turn_duration") {
                raw.duration_ms.map(|dur| ParsedLine {
                    kind: LineParsed::System { duration_ms: dur },
                    timestamp: ts,
                    cwd,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

// Accumulator for the line-merge loop. Carved out so both the
// sequential (fused) and parallel (collect-then-merge) paths in
// `parse_session` share the same logic.
struct SessionBuilder {
    messages: Vec<Message>,
    summary: Option<String>,
    first_user_msg: Option<String>,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    model: Option<String>,
    turn_count: usize,
    cwd: Option<String>,
}

impl SessionBuilder {
    fn with_capacity(line_estimate: usize) -> Self {
        Self {
            // ~1.3 messages per line for typical Claude logs (assistant
            // lines fan out into text+tool blocks); overshooting by 30%
            // is cheaper than reallocating mid-loop.
            messages: Vec::with_capacity(line_estimate * 13 / 10),
            summary: None,
            first_user_msg: None,
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            model: None,
            turn_count: 0,
            cwd: None,
        }
    }

    fn push(&mut self, parsed: ParsedLine) {
        let ts = parsed.timestamp;
        if self.cwd.is_none() {
            self.cwd = parsed.cwd;
        }
        match parsed.kind {
            LineParsed::User { parts } => {
                for (kind, text, tool_name) in parts {
                    if kind == MessageKind::User && self.first_user_msg.is_none() {
                        // Slice-truncate so a 50KB paste doesn't get
                        // cloned just to throw away 49.8KB of it.
                        self.first_user_msg = Some(truncated_copy(&text, 200));
                    }
                    if kind == MessageKind::ToolResult && text.is_empty() {
                        continue;
                    }
                    self.messages.push(Message {
                        timestamp: ts,
                        kind,
                        content: text,
                        tokens: None,
                        tool_name,
                        model: None,
                        message_id: None,
                    });
                }
                self.turn_count += 1;
            }
            LineParsed::Assistant {
                parts,
                model: mut msg_model,
                tokens,
                mut message_id,
            } => {
                if self.model.is_none() {
                    self.model.clone_from(&msg_model);
                }
                if let Some(ref t) = tokens {
                    self.total_input += t.input;
                    self.total_output += t.output;
                    self.total_cache_read += t.cache_read;
                    self.total_cache_create += t.cache_create;
                }
                // Peek-ahead trick: clone msg_model / message_id for
                // every non-empty part except the last, where we move.
                // For the typical single-part assistant line this lands
                // on `is_last` immediately and skips all string clones.
                let mut iter = parts
                    .into_iter()
                    .filter(|(_, t, _)| !t.is_empty())
                    .peekable();
                while let Some((kind, text, tool_name)) = iter.next() {
                    let is_last = iter.peek().is_none();
                    let (m, mid) = if is_last {
                        (msg_model.take(), message_id.take())
                    } else {
                        (msg_model.clone(), message_id.clone())
                    };
                    self.messages.push(Message {
                        timestamp: ts,
                        kind,
                        content: text,
                        tokens,
                        tool_name,
                        model: m,
                        message_id: mid,
                    });
                }
            }
            LineParsed::Summary(text) => {
                self.summary = Some(text);
            }
            LineParsed::System { duration_ms } => {
                self.messages.push(Message {
                    timestamp: ts,
                    kind: MessageKind::System,
                    content: format!("Turn completed in {:.1}s", duration_ms as f64 / 1000.0),
                    tokens: None,
                    tool_name: None,
                    model: None,
                    message_id: None,
                });
            }
        }
    }
}

// Slice-truncate a String into a fresh, short owned copy. Avoids
// cloning the whole input when we know the keeper portion is small.
fn truncated_copy(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 3);
    out.push_str(&s[..end]);
    out.push_str("...");
    out
}

#[allow(clippy::indexing_slicing)]
pub fn parse_session(path: &Path) -> Option<Session> {
    let file = fs::File::open(path).ok()?;
    // SAFETY: read-only mmap, file not modified during parse
    #[allow(unsafe_code)]
    let mmap = unsafe { memmap2::Mmap::map(&file).ok()? };
    let data = &*mmap;
    if data.is_empty() {
        return None;
    }

    // ≤10MB: fused single-pass loop. memchr_iter yields line ends, we
    // parse + merge inline. Skips the two intermediate Vecs
    // (`line_ranges` + `Vec<Option<ParsedLine>>`) the parallel path
    // needs. >10MB: collect line ranges, parse them in parallel, then
    // merge sequentially since order matters for first_user_msg / model.
    let mut b = SessionBuilder::with_capacity(data.len() / 600);
    if data.len() > 10_000_000 {
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(data.len() / 600);
        let mut start = 0;
        for pos in memchr::memchr_iter(b'\n', data) {
            if pos > start {
                ranges.push((start, pos));
            }
            start = pos + 1;
        }
        if start < data.len() {
            ranges.push((start, data.len()));
        }
        let parsed: Vec<Option<ParsedLine>> = ranges
            .par_iter()
            .map(|&(s, e)| parse_one_line(&data[s..e]))
            .collect();
        for p in parsed.into_iter().flatten() {
            b.push(p);
        }
    } else {
        let mut start = 0;
        for pos in memchr::memchr_iter(b'\n', data) {
            if pos > start {
                if let Some(p) = parse_one_line(&data[start..pos]) {
                    b.push(p);
                }
            }
            start = pos + 1;
        }
        if start < data.len() {
            if let Some(p) = parse_one_line(&data[start..]) {
                b.push(p);
            }
        }
    }
    let SessionBuilder {
        messages,
        summary,
        first_user_msg,
        total_input,
        total_output,
        total_cache_read,
        total_cache_create,
        model,
        turn_count,
        cwd,
    } = b;

    if messages.is_empty() {
        return None;
    }

    let started_at = messages.first().and_then(|m| m.timestamp);
    let ended_at = messages.last().and_then(|m| m.timestamp);

    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let msg_count = u32::try_from(messages.len()).unwrap_or(u32::MAX);

    Some(Session {
        id,
        file_path: path.to_path_buf(),
        messages,
        summary,
        first_user_msg,
        msg_count,
        cwd,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_read,
        total_cache_create,
        started_at,
        ended_at,
        turn_count,
        model,
        cost: 0.0,
        cost_input: 0.0,
        cost_output: 0.0,
        cost_cache_read: 0.0,
        cost_cache_create: 0.0,
    })
}

// ── Top-level loader (TUI/web/--json) ──
//
// Builds the Project/Session tree with header-only sessions: per-row
// metadata + canonical token/cost totals, no message content. Callers
// that need messages (TUI session view, web per-session JSON gen) lazy-
// load via `ensure_messages_loaded`.
//
// Cost/token figures come from `cache::per_session_totals` (the same
// canonical pipeline `daily`/`session`/web-rollup use), so this path
// agrees with the CLI usage reports to the cent.

// With CCAUDIT_PROF set, prints a one-line cache hit/miss summary to
// stderr so the user can tell cold runs from warm ones. Silent otherwise
// to keep TUI/web invocations clean.
#[allow(clippy::print_stderr)]
pub fn load_all_projects<S: crate::source::Source + ?Sized>(source: &S) -> Vec<Project> {
    let projects_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude").join("projects"),
        None => return vec![],
    };

    if !projects_dir.exists() {
        return vec![];
    }

    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new(); // (project_dir, jsonl_file)
    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if let Ok(sub) = fs::read_dir(&dir) {
                for f in sub.flatten() {
                    let p = f.path();
                    // Only top-level JSONL files (skip subagent dirs, plugin dirs, etc.)
                    if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        files.push((dir.clone(), p));
                    }
                }
            }
        }
    }

    if files.is_empty() {
        return vec![];
    }

    // Canonical aggregation cache — owns token/cost totals + last-active
    // timestamps. Handles its own incremental rebuild from JSONL.
    let cache = crate::cache::load(source);
    let totals = crate::cache::per_session_totals(&cache, source);

    let cache_hits = AtomicUsize::new(0);
    let cache_misses = AtomicUsize::new(0);

    struct ParsedFile {
        dir: PathBuf,
        path_hash: u64,
        session: Session,
    }
    let parsed: Vec<ParsedFile> = files
        .par_iter()
        .filter_map(|(dir, file)| {
            let path_hash = crate::source::path_hash(file);
            // Header-only fast path — skips deserializing the messages
            // blob, which is what made warm cold-starts expensive.
            if let Some(mut session) = try_load_cached_header(file) {
                session.file_path.clone_from(file);
                let _ = cache_hits.fetch_add(1, Ordering::Relaxed);
                return Some(ParsedFile {
                    dir: dir.clone(),
                    path_hash,
                    session,
                });
            }
            let _ = cache_misses.fetch_add(1, Ordering::Relaxed);
            // Cache miss: full parse, then split-write so subsequent runs
            // hit the header-only path above.
            let session = parse_session(file)?;
            save_to_cache(file, &session);
            // Keep header in memory but drop messages — consumers that need
            // them will re-read from `.msgs`.
            let mut header_only = session;
            header_only.messages = Vec::new();
            Some(ParsedFile {
                dir: dir.clone(),
                path_hash,
                session: header_only,
            })
        })
        .collect();

    if std::env::var_os("CCAUDIT_PROF").is_some() {
        let hits = cache_hits.load(Ordering::Relaxed);
        let misses = cache_misses.load(Ordering::Relaxed);
        eprintln!("cache: {hits} hits, {misses} misses");
    }

    let mut parsed = parsed;
    for p in &mut parsed {
        if let Some(t) = totals.get(&p.path_hash) {
            let session = &mut p.session;
            session.total_input_tokens = t.input;
            session.total_output_tokens = t.output;
            session.total_cache_read = t.cache_read;
            session.total_cache_create = t.cache_create;
            session.cost = t.cost;
            session.cost_input = t.cost_input;
            session.cost_output = t.cost_output;
            session.cost_cache_read = t.cost_cache_read;
            session.cost_cache_create = t.cost_cache_create;
            // ended_at = last billable line ts (canonical "last active").
            if t.last_ts > 0 {
                session.ended_at = DateTime::from_timestamp(t.last_ts, 0);
            }
        }
    }

    let mut project_map: FxHashMap<PathBuf, Vec<Session>> = FxHashMap::default();
    for p in parsed {
        project_map.entry(p.dir).or_default().push(p.session);
    }

    let mut projects: Vec<Project> = project_map
        .into_iter()
        .map(|(dir, mut sessions)| {
            sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
            // Prefer the unambiguous cwd recorded inside any session in this
            // project dir — the dash-encoded dir name loses real hyphens.
            let pretty_name = if let Some(c) = sessions.iter().find_map(|s| s.cwd.as_deref()) {
                crate::source::prettify_cwd(c)
            } else {
                let name = dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                crate::source::claude_code::prettify_project_name(name)
            };

            // All four token columns — `Session::total_tokens()` uses the
            // same sum, and every renderer (CLI, TUI, web JS) treats
            // "total tokens" as input + output + cache read + cache create.
            let total_tokens: u64 = sessions.iter().map(Session::total_tokens).sum();
            let last_active = sessions.iter().filter_map(|s| s.ended_at).max();
            let total_msgs: u64 = sessions.iter().map(|s| u64::from(s.msg_count)).sum();
            let total_dur_ms: u64 = sessions
                .iter()
                .filter_map(|s| match (s.started_at, s.ended_at) {
                    (Some(a), Some(b)) if b > a => Some((b - a).num_milliseconds().max(0) as u64),
                    _ => None,
                })
                .sum();
            let total_cost: f64 = sessions.iter().map(|s| s.cost).sum();

            Project {
                name: pretty_name,
                sessions,
                total_tokens,
                last_active,
                total_msgs,
                total_dur_ms,
                total_cost,
            }
        })
        .collect();

    projects.sort_by_key(|p| std::cmp::Reverse(p.last_active));
    projects
}

impl Session {
    /// Canonical display-name fallback chain: summary > first user
    /// message > session id. Any caller that needs the user-visible
    /// title for a session goes through this — keeps the TUI list,
    /// the web sidebar, and the cache's stored `display_name` aligned.
    pub fn display_name(&self) -> &str {
        if let Some(ref s) = self.summary {
            s.as_str()
        } else if let Some(ref m) = self.first_user_msg {
            m.as_str()
        } else {
            &self.id
        }
    }

    pub const fn total_tokens(&self) -> u64 {
        self.total_input_tokens
            + self.total_output_tokens
            + self.total_cache_read
            + self.total_cache_create
    }
}

impl std::fmt::Display for MessageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageKind::User => write!(f, "USER"),
            MessageKind::Assistant => write!(f, "ASST"),
            MessageKind::ToolUse => write!(f, "TOOL"),
            MessageKind::ToolResult => write!(f, "RSLT"),
            MessageKind::Thinking => write!(f, "THNK"),
            MessageKind::System => write!(f, "SYS"),
        }
    }
}
