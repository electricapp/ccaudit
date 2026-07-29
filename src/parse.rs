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
    /// (mtime, size) captured BEFORE the JSONL was mapped. `save_to_cache`
    /// must stamp the cache with this, not a fresh stat: a live session can
    /// be appended between parse and save, and fingerprinting afterwards
    /// would mark stale content as fresh — permanently, if the session then
    /// goes idle.
    #[serde(skip)]
    pub fingerprint: Option<(u64, u64)>,
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
    /// Total cache-creation tokens, both TTLs.
    pub cache_create: u64,
    /// Portion of `cache_create` written at the 1-hour TTL, which bills
    /// at a higher rate. `serde(default)` so a per-session cache blob
    /// written before this field existed still deserializes.
    #[serde(default)]
    pub cache_create_1h: u64,
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
// blob for every session just to call `.messages.len()` dominates
// warm runs.

fn cache_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("ccaudit-cache"))
}

/// `(mtime_secs, size)` for `path`, or `None` if it can't be stat'd.
///
/// Public so a provider's transcript parser can capture the fingerprint
/// BEFORE it reads the file — see the `Session::fingerprint` contract.
pub fn file_fingerprint(path: &Path) -> Option<(u64, u64)> {
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

// Bump whenever the Session header or Message struct changes shape, the
// on-disk encoding changes, or the parser changes what a Session
// contains — invalidation is fingerprint-based, so a stale blob would
// otherwise stay "fresh" forever.
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

/// Load messages for `path` into `session.messages` if not already there.
///
/// Falls back to re-reading the provider's log when the per-session
/// cache is missing or stale; `false` means even that produced nothing.
pub fn ensure_messages_loaded<S: crate::source::Source + ?Sized>(
    source: &S,
    session: &mut Session,
    path: &Path,
) -> bool {
    if !session.messages.is_empty() {
        return true;
    }
    if let Some(msgs) = load_messages_for(path) {
        session.messages = msgs;
        return true;
    }
    if let Some(s) = source.parse_messages(path) {
        session.messages = s.messages;
        return true;
    }
    false
}

fn save_to_cache(path: &Path, session: &Session) {
    let Some(dir) = cache_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let key = cache_key(path);
    // Prefer the fingerprint captured before the parse mmap'd the file
    // (see `Session::fingerprint`); a fresh stat here would attribute
    // concurrently-appended bytes to content that predates them.
    let Some(fp) = session.fingerprint.or_else(|| file_fingerprint(path)) else {
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

// ── JSONL deserialization types ──

// `msg_type` / `subtype` / `timestamp` are matched or parsed and then
// dropped, so they borrow from the line slice (`Cow` still tolerates
// escaped strings, which must allocate). `cwd` and the `RawMessage`
// fields are stored beyond the line's lifetime and stay owned.
#[derive(Deserialize)]
struct RawLine<'a> {
    #[serde(rename = "type", borrow)]
    msg_type: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    subtype: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    timestamp: Option<std::borrow::Cow<'a, str>>,
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
    /// Per-TTL split of `cache_creation_input_tokens`. Anthropic bills
    /// the 1-hour tier at 2× input against the 5-minute tier's 1.25×, so
    /// the flat total alone can't be priced. Absent on older logs, where
    /// only the 5-minute tier existed.
    cache_creation: Option<RawCacheCreation>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCacheCreation {
    ephemeral_1h_input_tokens: u64,
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

fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    // Fast path for "2026-03-30T14:10:41.157Z" format (fixed layout).
    // Gated on a trailing 'Z' so a timestamp carrying a numeric offset
    // ("…+02:00") is NOT misread as UTC — it falls through to chrono's
    // full parser, which honors the offset. Claude Code / Codex always
    // emit the Z form, so the fast path still covers every real line.
    // Any fast-path failure (including shapes chrono accepts but this
    // layout doesn't, e.g. a leap-second ":60") falls through rather
    // than rejecting the line outright.
    fast_parse_timestamp(s.as_bytes()).or_else(|| s.parse::<DateTime<Utc>>().ok())
}

#[allow(clippy::indexing_slicing)] // indices are bounds-checked by b.len() >= 20
fn fast_parse_timestamp(b: &[u8]) -> Option<DateTime<Utc>> {
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b.last() != Some(&b'Z')
    {
        return None;
    }
    let year = i32::try_from(fast_parse_u32(&b[0..4])?).ok()?;
    let month = fast_parse_u32(&b[5..7])?;
    let day = fast_parse_u32(&b[8..10])?;
    let hour = fast_parse_u32(&b[11..13])?;
    let min = fast_parse_u32(&b[14..16])?;
    let sec = fast_parse_u32(&b[17..19])?;
    let ndt = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, min, sec)?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
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

fn extract_text_content(
    content: RawContent,
    from_user: bool,
) -> Vec<(MessageKind, String, Option<String>)> {
    // Text carries the kind of the line it came from: a `text` block on a
    // `type:"user"` line is the user's prompt (block-array form shows up
    // whenever the prompt has attachments), not assistant output.
    let text_kind = if from_user {
        MessageKind::User
    } else {
        MessageKind::Assistant
    };
    match content {
        RawContent::Text(s) => vec![(text_kind, s, None)],
        RawContent::Blocks(blocks) => {
            let mut out = Vec::with_capacity(blocks.len());
            for b in blocks {
                match b {
                    RawBlock::Text { text } if !text.is_empty() => {
                        out.push((text_kind.clone(), text, None));
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

    // JSON parser switch. Default deserializes straight off the immutable
    // mmap'd slice (RawLine's Cow fields borrow from it); with
    // `--features simd-json`, the slice is copied into a mutable buffer
    // that simd-json's SIMD-accelerated parser scribbles in place — the
    // borrows then point into that local buffer, which outlives `raw`.
    #[cfg(feature = "simd-json")]
    let mut simd_buf = line.to_vec();
    #[cfg(feature = "simd-json")]
    let raw: RawLine = simd_json::serde::from_slice(&mut simd_buf).ok()?;
    #[cfg(not(feature = "simd-json"))]
    let raw: RawLine = serde_json::from_slice(line).ok()?;

    let ts = raw.timestamp.as_deref().and_then(parse_timestamp);
    let msg_type = raw.msg_type.as_deref().unwrap_or("");
    let cwd = raw.cwd.filter(|s| !s.is_empty());

    match msg_type {
        "user" => {
            let msg = raw.message?;
            let content = msg.content?;
            let parts = extract_text_content(content, true);
            Some(ParsedLine {
                kind: LineParsed::User { parts },
                timestamp: ts,
                cwd,
            })
        }
        "assistant" => {
            let msg = raw.message?;
            let content = msg.content?;
            let parts = extract_text_content(content, false);
            let tokens = msg.usage.as_ref().map(|u| TokenUsage {
                input: u.input_tokens.unwrap_or(0),
                output: u.output_tokens.unwrap_or(0),
                cache_read: u.cache_read_input_tokens.unwrap_or(0),
                cache_create: u.cache_creation_input_tokens.unwrap_or(0),
                cache_create_1h: u
                    .cache_creation
                    .as_ref()
                    .map_or(0, |c| c.ephemeral_1h_input_tokens),
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
    // FNV hash of the last assistant line's message_id whose usage was
    // added to the totals. Messages streamed as several JSONL lines repeat
    // the same id and usage on each line; counting every line would
    // double/triple the totals (`to_parsed_session` coalesces by the same
    // consecutive-id rule).
    last_usage_id_hash: Option<u64>,
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
            last_usage_id_hash: None,
        }
    }

    fn push(&mut self, parsed: ParsedLine) {
        let ts = parsed.timestamp;
        if self.cwd.is_none() {
            self.cwd = parsed.cwd;
        }
        match parsed.kind {
            LineParsed::User { parts } => {
                // A "user" line whose parts are all tool_results is the
                // harness returning tool output, not a human turn.
                let mut is_human_turn = false;
                for (kind, text, tool_name) in parts {
                    if kind == MessageKind::User {
                        is_human_turn = true;
                        if self.first_user_msg.is_none() {
                            // Slice-truncate so a 50KB paste doesn't get
                            // cloned just to throw away 49.8KB of it.
                            self.first_user_msg = Some(truncated_copy(&text, 200));
                        }
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
                if is_human_turn {
                    self.turn_count += 1;
                }
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
                // Add usage once per API call, not once per JSONL line:
                // consecutive lines sharing a message_id are one streamed
                // message with the usage repeated on each line.
                let id_hash = message_id
                    .as_deref()
                    .map(|id| crate::source::fnv1a(id.as_bytes()));
                let is_repeat = id_hash.is_some() && id_hash == self.last_usage_id_hash;
                if let Some(ref t) = tokens {
                    if !is_repeat {
                        self.total_input += t.input;
                        self.total_output += t.output;
                        self.total_cache_read += t.cache_read;
                        self.total_cache_create += t.cache_create;
                    }
                }
                self.last_usage_id_hash = id_hash;
                // Keep tool_use parts even when their rendered input is
                // empty (unmodeled tools) — dropping them drops the
                // line's token usage from `to_parsed_session` entirely.
                // Id-less lines get usage on the FIRST part only, since
                // downstream dedup can't coalesce parts without an id.
                let mut usage_left = tokens;
                let mut iter = parts
                    .into_iter()
                    .filter(|(k, t, _)| *k == MessageKind::ToolUse || !t.is_empty())
                    .peekable();
                // Peek-ahead trick: clone msg_model / message_id for
                // every part except the last, where we move. For the
                // typical single-part assistant line this lands on
                // `is_last` immediately and skips all string clones.
                let mut pushed_any = false;
                while let Some((kind, text, tool_name)) = iter.next() {
                    pushed_any = true;
                    let is_last = iter.peek().is_none();
                    let (m, mid) = if is_last {
                        (msg_model.take(), message_id.take())
                    } else {
                        (msg_model.clone(), message_id.clone())
                    };
                    let part_tokens = if mid.is_some() {
                        tokens
                    } else {
                        usage_left.take()
                    };
                    self.messages.push(Message {
                        timestamp: ts,
                        kind,
                        content: text,
                        tokens: part_tokens,
                        tool_name,
                        model: m,
                        message_id: mid,
                    });
                }
                // A line whose blocks are all unmodeled (redacted_thinking,
                // image-only, server tools) still bills tokens — record an
                // empty placeholder so the usage survives into
                // `to_parsed_session` instead of vanishing from aggregates.
                if !pushed_any && tokens.is_some() {
                    self.messages.push(Message {
                        timestamp: ts,
                        kind: MessageKind::Assistant,
                        content: String::new(),
                        tokens,
                        tool_name: None,
                        model: msg_model.take(),
                        message_id: message_id.take(),
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
pub(crate) fn truncated_copy(s: &str, max_bytes: usize) -> String {
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

/// Parse a session, treating a readable file with no messages as `None`.
///
/// The TUI/web project tree uses this so contentless sessions don't
/// clutter listings. The cache layer must use `parse_session_allow_empty`
/// instead — see the `Source::parse_session` contract.
pub fn parse_session(path: &Path) -> Option<Session> {
    parse_session_allow_empty(path).filter(|s| !s.messages.is_empty())
}

/// Like `parse_session`, but a readable file with zero parseable messages
/// yields `Some(empty Session)` rather than `None`.
///
/// The aggregation cache validates by matching its session count against
/// the scanned-file count, so returning `None` for a file that keeps
/// being scanned would force a full rebuild on every run.
#[allow(clippy::indexing_slicing)]
pub fn parse_session_allow_empty(path: &Path) -> Option<Session> {
    // Fingerprint BEFORE mapping: an append racing the parse must leave
    // the cache entry stale (old fingerprint, old content), never stamp
    // the new (mtime,size) onto content that predates it.
    let fingerprint = file_fingerprint(path)?;
    if fingerprint.1 == 0 {
        // Zero-byte file: readable but empty (mmap would reject a
        // zero-length map anyway).
        return Some(build_session(
            path,
            SessionBuilder::with_capacity(0),
            fingerprint,
        ));
    }
    let file = fs::File::open(path).ok()?;
    // SAFETY: read-only map. Concurrent appends are benign — the mapping
    // length is fixed at map time. A truncation mid-parse would SIGBUS;
    // Claude Code session files are append-only in practice, so this is
    // accepted rather than paying a defensive full read.
    #[allow(unsafe_code)]
    let mmap = unsafe { memmap2::Mmap::map(&file).ok()? };
    let mut data = &*mmap;
    // Strip a UTF-8 BOM (files round-tripped through editors add one) so
    // the first line's JSON parse doesn't silently fail.
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        data = &data[3..];
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
    Some(build_session(path, b, fingerprint))
}

fn build_session(path: &Path, b: SessionBuilder, fingerprint: (u64, u64)) -> Session {
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
        ..
    } = b;

    let started_at = messages.first().and_then(|m| m.timestamp);
    let ended_at = messages.last().and_then(|m| m.timestamp);

    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let msg_count = u32::try_from(messages.len()).unwrap_or(u32::MAX);

    Session {
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
        fingerprint: Some(fingerprint),
    }
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
pub fn load_all_projects<S: crate::source::Source + ?Sized>(source: &S) -> Vec<Project> {
    load_all_projects_with_cache(source).0
}

/// `load_all_projects`, also handing back the aggregation cache it built
/// internally — callers that need both (the web generator) would
/// otherwise pay a second full scan + validate pass.
#[allow(clippy::print_stderr)]
pub fn load_all_projects_with_cache<S: crate::source::Source + ?Sized>(
    source: &S,
) -> (Vec<Project>, crate::cache::LoadedCache) {
    // One scan_sources() pass covers both this file list and the cache
    // load below (and gets the darwin bulk-scan path for free).
    let files = source.scan_sources();
    if files.is_empty() {
        return (vec![], crate::cache::load(source));
    }

    // Canonical aggregation cache — owns token/cost totals + last-active
    // timestamps. Handles its own incremental rebuild from JSONL.
    let cache = crate::cache::load(source);
    let totals = crate::cache::per_session_totals(&cache, source);

    let cache_hits = AtomicUsize::new(0);
    let cache_misses = AtomicUsize::new(0);

    struct ParsedFile {
        /// Provider-supplied grouping key — see `Source::project_key`.
        bucket: String,
        path_hash: u64,
        session: Session,
    }
    let parsed: Vec<ParsedFile> = files
        .par_iter()
        .filter_map(|src| {
            let file = &src.path;
            let path_hash = src.path_hash;
            // Header-only fast path — skips deserializing the messages
            // blob, which is what made warm cold-starts expensive.
            if let Some(mut session) = try_load_cached_header(file) {
                // Contentless sessions live in the cache (the aggregation
                // layer needs their count) but have nothing to list.
                if session.msg_count == 0 {
                    return None;
                }
                session.file_path.clone_from(file);
                let _ = cache_hits.fetch_add(1, Ordering::Relaxed);
                return Some(ParsedFile {
                    bucket: source.project_key(file, session.cwd.as_deref()),
                    path_hash,
                    session,
                });
            }
            let _ = cache_misses.fetch_add(1, Ordering::Relaxed);
            // Cache miss: full parse, then split-write so subsequent runs
            // hit the header-only path above. Empty sessions are cached
            // (so the next run's header path skips them cheaply) but not
            // listed.
            let session = source.parse_messages(file)?;
            save_to_cache(file, &session);
            if session.messages.is_empty() {
                return None;
            }
            let bucket = source.project_key(file, session.cwd.as_deref());
            // Keep header in memory but drop messages — consumers that need
            // them will re-read from `.msgs`.
            let mut header_only = session;
            header_only.messages = Vec::new();
            Some(ParsedFile {
                bucket,
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

    let mut project_map: FxHashMap<String, Vec<Session>> = FxHashMap::default();
    for p in parsed {
        project_map.entry(p.bucket).or_default().push(p.session);
    }

    let mut projects: Vec<Project> = project_map
        .into_iter()
        .map(|(bucket, mut sessions)| {
            sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
            // Prefer the unambiguous cwd recorded inside any session in this
            // project dir — the dash-encoded dir name loses real hyphens.
            let pretty_name = if let Some(c) = sessions.iter().find_map(|s| s.cwd.as_deref()) {
                crate::source::prettify_cwd(c)
            } else {
                let name = Path::new(&bucket)
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
    (projects, cache)
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
