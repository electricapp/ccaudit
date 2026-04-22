use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    pub summary: Option<String>,
    pub first_user_msg: Option<String>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read: u64,
    pub total_cache_create: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub turn_count: usize,
    pub model: Option<String>,
    // Deduped cost, priced per-message model. Set during load_all_projects
    // after cross-session dedup so it reflects true user spend for this
    // session's unique messages. Models can change mid-session (e.g. /fast,
    // compaction uses a different model), so we can't just price the
    // session-level totals against a single `model` field.
    pub cost: f64,
    // Per-token-type dollar costs. Sum to `cost`. Computed alongside
    // `cost` in the same per-message loop so mid-session model switches
    // stay accurate per column. Powers the cost-breakdown tooltip in
    // the web view.
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
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

// ── Cache ──

// Per-session cache: each session is stored as an individual bincode file
// in ~/.claude/ccaudit-cache/<hash>.bin with a companion .meta (mtime+size).
// This avoids loading/deserializing a monolithic 28MB blob on every run.

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
    // Simple hash of the full path for filename
    let s = path.to_string_lossy();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

const CACHE_VERSION: u8 = 6; // bump when Message/Session struct changes

#[derive(Serialize, Deserialize)]
struct CacheMeta {
    version: u8,
    mtime_secs: u64,
    size: u64,
}

fn try_load_cached(path: &Path) -> Option<Session> {
    let dir = cache_dir()?;
    let key = cache_key(path);
    let (cur_mtime, cur_size) = file_fingerprint(path)?;

    // Read meta
    let meta_path = dir.join(format!("{key}.meta"));
    let meta_bytes = fs::read(&meta_path).ok()?;
    let meta: CacheMeta = bincode::deserialize(&meta_bytes).ok()?;

    if meta.version != CACHE_VERSION || meta.mtime_secs != cur_mtime || meta.size != cur_size {
        return None;
    }

    // Read cached session
    let data_path = dir.join(format!("{key}.bin"));
    let data = fs::read(&data_path).ok()?;
    bincode::deserialize(&data).ok()
}

fn save_to_cache(path: &Path, session: &Session) {
    let Some(dir) = cache_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let key = cache_key(path);
    let Some(fp) = file_fingerprint(path) else {
        return;
    };

    let meta = CacheMeta {
        version: CACHE_VERSION,
        mtime_secs: fp.0,
        size: fp.1,
    };
    if let Ok(meta_bytes) = bincode::serialize(&meta) {
        let _ = fs::write(dir.join(format!("{key}.meta")), meta_bytes);
    }
    if let Ok(data) = bincode::serialize(session) {
        let _ = fs::write(dir.join(format!("{key}.bin")), data);
    }
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
}

#[derive(Deserialize)]
struct RawMessage {
    id: Option<String>,
    content: Option<serde_json::Value>,
    model: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
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

fn truncate_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

#[allow(clippy::indexing_slicing)]
fn extract_text_content(content: &serde_json::Value) -> Vec<(MessageKind, String, Option<String>)> {
    if let Some(s) = content.as_str() {
        return vec![(MessageKind::User, s.to_string(), None)];
    }
    let Some(arr) = content.as_array() else {
        return vec![];
    };
    let mut results = Vec::with_capacity(arr.len());
    for item in arr {
        let block_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        results.push((MessageKind::Assistant, text.to_string(), None));
                    }
                }
            }
            "thinking" => {
                if let Some(text) = item.get("thinking").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        results.push((MessageKind::Thinking, text.to_string(), None));
                    }
                }
            }
            "tool_use" => {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let input_str = item
                    .get("input")
                    .map(|v| format_tool_input(name, v))
                    .unwrap_or_default();
                results.push((MessageKind::ToolUse, input_str, Some(name.to_string())));
            }
            "tool_result" => {
                let text = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    results.push((MessageKind::ToolResult, truncate_str(text, 500), None));
                }
            }
            _ => {}
        }
    }
    results
}

fn format_tool_input(tool: &str, input: &serde_json::Value) -> String {
    let s = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match tool {
        "Bash" => match input.get("description").and_then(|v| v.as_str()) {
            Some(d) => format!("$ {}\n  # {d}", s("command")),
            None => format!("$ {}", s("command")),
        },
        "Read" | "Write" | "Edit" => format!("{} {}", tool.to_lowercase(), s("file_path")),
        "Glob" | "Grep" => format!("{} {}", tool.to_lowercase(), s("pattern")),
        "Agent" => {
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("agent");
            format!("agent: {desc}")
        }
        _ => truncate_str(&format!("{input}"), 200),
    }
}

// ── Core parser ──

// Parsed data from a single JSONL line, used for parallel intra-file parsing
struct ParsedLine {
    kind: LineParsed,
    timestamp: Option<DateTime<Utc>>,
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
    // Quick reject: scan for any of our target type strings.
    // The "type" field position varies (byte 1-200+), so we scan the whole line
    // but only do a single memmem for the common prefix.
    if memchr::memmem::find(line, b"\"type\":\"user\"").is_none()
        && memchr::memmem::find(line, b"\"type\":\"assistant\"").is_none()
        && memchr::memmem::find(line, b"\"type\":\"summary\"").is_none()
        && memchr::memmem::find(line, b"\"type\":\"system\"").is_none()
    {
        return None;
    }

    let raw: RawLine = serde_json::from_slice(line).ok()?;
    let ts = raw.timestamp.as_deref().and_then(parse_timestamp);
    let msg_type = raw.msg_type.as_deref().unwrap_or("");

    match msg_type {
        "user" => {
            let msg = raw.message.as_ref()?;
            let content = msg.content.as_ref()?;
            let parts = extract_text_content(content);
            Some(ParsedLine {
                kind: LineParsed::User { parts },
                timestamp: ts,
            })
        }
        "assistant" => {
            let msg = raw.message.as_ref()?;
            let content = msg.content.as_ref()?;
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
                    model: msg.model.clone(),
                    tokens,
                    message_id: msg.id.clone(),
                },
                timestamp: ts,
            })
        }
        "summary" => {
            let msg = raw.message.as_ref()?;
            let content = msg.content.as_ref()?;
            let text = if let Some(s) = content.as_str() {
                s.to_string()
            } else if let Some(arr) = content.as_array() {
                arr.iter()
                    .find_map(|item| item.get("text").and_then(|v| v.as_str()))
                    .map(String::from)?
            } else {
                return None;
            };
            Some(ParsedLine {
                kind: LineParsed::Summary(text),
                timestamp: ts,
            })
        }
        "system" => {
            if raw.subtype.as_deref() == Some("turn_duration") {
                raw.duration_ms.map(|dur| ParsedLine {
                    kind: LineParsed::System { duration_ms: dur },
                    timestamp: ts,
                })
            } else {
                None
            }
        }
        _ => None,
    }
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

    // Find line boundaries using SIMD-accelerated memchr
    let line_ranges: Vec<(usize, usize)> = {
        let mut ranges = Vec::new();
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
        ranges
    };

    // Parse lines in parallel for large files (>10MB), sequential otherwise
    let parsed_lines: Vec<Option<ParsedLine>> = if data.len() > 10_000_000 {
        line_ranges
            .par_iter()
            .map(|&(s, e)| parse_one_line(&data[s..e]))
            .collect()
    } else {
        line_ranges
            .iter()
            .map(|&(s, e)| parse_one_line(&data[s..e]))
            .collect()
    };

    // Merge results sequentially (order matters for first_user_msg, model, etc.)
    let mut messages = Vec::new();
    let mut summary: Option<String> = None;
    let mut first_user_msg: Option<String> = None;
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_cache_create = 0u64;
    let mut model: Option<String> = None;
    let mut turn_count = 0usize;

    for parsed in parsed_lines.into_iter().flatten() {
        let ts = parsed.timestamp;
        match parsed.kind {
            LineParsed::User { parts } => {
                for (kind, text, tool_name) in parts {
                    if kind == MessageKind::User && first_user_msg.is_none() {
                        first_user_msg = Some(truncate_str(&text, 200));
                    }
                    if kind == MessageKind::ToolResult && text.is_empty() {
                        continue;
                    }
                    messages.push(Message {
                        timestamp: ts,
                        kind,
                        content: text,
                        tokens: None,
                        tool_name,
                        model: None,
                        message_id: None,
                    });
                }
                turn_count += 1;
            }
            LineParsed::Assistant {
                parts,
                model: msg_model,
                tokens,
                message_id,
            } => {
                if model.is_none() {
                    model.clone_from(&msg_model);
                }
                if let Some(ref t) = tokens {
                    total_input += t.input;
                    total_output += t.output;
                    total_cache_read += t.cache_read;
                    total_cache_create += t.cache_create;
                }
                for (kind, text, tool_name) in parts {
                    if text.is_empty() {
                        continue;
                    }
                    messages.push(Message {
                        timestamp: ts,
                        kind,
                        content: text,
                        tokens,
                        tool_name,
                        model: msg_model.clone(),
                        message_id: message_id.clone(),
                    });
                }
            }
            LineParsed::Summary(text) => {
                summary = Some(text);
            }
            LineParsed::System { duration_ms } => {
                messages.push(Message {
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

    Some(Session {
        id,
        messages,
        summary,
        first_user_msg,
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
// Produces the full Project/Session/Message tree with cross-session
// dedup applied so session.cost matches the usage-report numbers.
// The usage reports themselves don't call this path — they use the
// much faster `cache::load()` directly.

pub fn load_all_projects() -> Vec<Project> {
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

    let cache_hits = AtomicUsize::new(0);
    let cache_misses = AtomicUsize::new(0);

    let parsed: Vec<(PathBuf, Session)> = files
        .par_iter()
        .filter_map(|(dir, file)| {
            if let Some(session) = try_load_cached(file) {
                let _ = cache_hits.fetch_add(1, Ordering::Relaxed);
                return Some((dir.clone(), session));
            }
            let _ = cache_misses.fetch_add(1, Ordering::Relaxed);
            let session = parse_session(file)?;
            save_to_cache(file, &session);
            Some((dir.clone(), session))
        })
        .collect();

    let hits = cache_hits.load(Ordering::Relaxed);
    let misses = cache_misses.load(Ordering::Relaxed);
    eprintln!("cache: {hits} hits, {misses} misses");

    // Cross-session dedup. Claude Code checkpoints/resumes a session by
    // copying prior assistant lines (same message_id) into a new JSONL
    // file. Without dedup, those tokens get counted once per session
    // they appear in — roughly doubling totals. Walk sessions in
    // chronological order and rewrite each session's totals to exclude
    // messages whose IDs were already seen earlier.
    let mut parsed = parsed;
    parsed.sort_by_key(|(_, s)| s.started_at);
    let mut seen_global: HashSet<String> = HashSet::new();
    for (_, session) in &mut parsed {
        let (
            input_tokens,
            output_tokens,
            cache_write_tokens,
            cache_read_tokens,
            cost,
            cost_input,
            cost_output,
            cost_cache_write,
            cost_cache_read,
        ) = {
            let mut input_tokens = 0u64;
            let mut output_tokens = 0u64;
            let mut cache_write_tokens = 0u64;
            let mut cache_read_tokens = 0u64;
            let mut cost_input = 0.0_f64;
            let mut cost_output = 0.0_f64;
            let mut cost_cache_write = 0.0_f64;
            let mut cost_cache_read = 0.0_f64;
            let mut seen_intra: HashSet<&str> = HashSet::new();
            let fallback_model = session.model.as_deref();
            for msg in &session.messages {
                let Some(tokens) = &msg.tokens else { continue };
                if let Some(id) = msg.message_id.as_deref() {
                    if !seen_intra.insert(id) {
                        continue;
                    }
                    if !seen_global.insert(id.to_string()) {
                        continue;
                    }
                }
                input_tokens += tokens.input;
                output_tokens += tokens.output;
                cache_write_tokens += tokens.cache_create;
                cache_read_tokens += tokens.cache_read;
                // Price per-message model so mid-session model switches
                // (e.g. /fast, compaction) are accounted for correctly.
                // Split into the four column costs alongside the total
                // so the web view can render a real breakdown tooltip
                // without re-pricing on the JS side.
                let model = msg.model.as_deref().or(fallback_model);
                use crate::source::Source as _;
                let p = crate::source::claude_code::ClaudeCode.price(model);
                cost_input += (tokens.input as f64) * p.input / 1_000_000.0;
                cost_output += (tokens.output as f64) * p.output / 1_000_000.0;
                cost_cache_write += (tokens.cache_create as f64) * p.cache_write / 1_000_000.0;
                cost_cache_read += (tokens.cache_read as f64) * p.cache_read / 1_000_000.0;
            }
            let cost = cost_input + cost_output + cost_cache_write + cost_cache_read;
            (
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_read_tokens,
                cost,
                cost_input,
                cost_output,
                cost_cache_write,
                cost_cache_read,
            )
        };
        session.total_input_tokens = input_tokens;
        session.total_output_tokens = output_tokens;
        session.total_cache_create = cache_write_tokens;
        session.total_cache_read = cache_read_tokens;
        session.cost = cost;
        session.cost_input = cost_input;
        session.cost_output = cost_output;
        session.cost_cache_create = cost_cache_write;
        session.cost_cache_read = cost_cache_read;
    }

    let mut project_map: HashMap<PathBuf, Vec<Session>> = HashMap::new();
    for (dir, session) in parsed {
        project_map.entry(dir).or_default().push(session);
    }

    let mut projects: Vec<Project> = project_map
        .into_iter()
        .map(|(dir, mut sessions)| {
            sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
            let name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let pretty_name = crate::source::claude_code::prettify_project_name(name);

            let total_tokens: u64 = sessions
                .iter()
                .map(|s| s.total_input_tokens + s.total_output_tokens)
                .sum();
            let last_active = sessions.iter().filter_map(|s| s.ended_at).max();

            Project {
                name: pretty_name,
                sessions,
                total_tokens,
                last_active,
            }
        })
        .collect();

    projects.sort_by_key(|p| std::cmp::Reverse(p.last_active));
    projects
}

#[cfg(any(feature = "tui", feature = "web"))]
impl Session {
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
        self.total_input_tokens + self.total_output_tokens
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
