// Claude Code `Source` implementation.
//
// Everything provider-specific about Anthropic's Claude Code logs lives
// here: where JSONL files are, how to parse them, how models are priced
// and named. Adding a new provider means adding a sibling of this file;
// no other layer needs to change.

use super::{ParsedLine, ParsedSession, Pricing, Source, SourceFile, day_from_ts, fnv1a};
use crate::parse::{self, Message, MessageKind, Session};
use std::borrow::Cow;
#[cfg(target_os = "macos")]
use std::fs;
use std::path::{Path, PathBuf};

pub struct ClaudeCode;

impl Source for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn logs_dir(&self) -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".claude").join("projects"))
    }

    // cache_path: default impl composes {cache_root}/{id}.db for us.

    fn scan_sources(&self) -> Vec<SourceFile> {
        let Some(dir) = self.logs_dir() else {
            return vec![];
        };
        // Opt-in fast path: macOS getattrlistbulk batches readdir + stat
        // into one kernel round-trip per directory, shaving ~500μs off
        // the hot path (~2.8ms → ~2.1ms on a typical project tree).
        // Set CCAUDIT_BULK_SCAN=1 to try it; any FFI error falls back
        // silently to the portable default_scan path.
        #[cfg(target_os = "macos")]
        if std::env::var_os("CCAUDIT_BULK_SCAN").is_some() {
            if let Some(out) = scan_with_bulk(&dir) {
                return out;
            }
        }
        super::default_scan(&dir)
    }

    fn parse_session(&self, src: &SourceFile) -> Option<ParsedSession> {
        let session = parse::parse_session(&src.path)?;
        Some(to_parsed_session(&src.path, src, &session))
    }

    fn price(&self, model: Option<&str>) -> &Pricing {
        // 1. Try the user's refreshed LiteLLM cache (if present). This
        //    matches ccusage's approach and keeps prices current without
        //    a code change. Multiple name variants are tried (exact +
        //    `anthropic/` prefix + date-stripped form) to cover how
        //    LiteLLM tends to key Claude models.
        if let Some(lookup) = super::prices::get() {
            if let Some(name) = model {
                let candidates = claude_name_candidates(name);
                if let Some(p) = lookup.lookup(&candidates) {
                    return p;
                }
            }
        }
        // 2. Fall back to the hardcoded table (March 2026 prices).
        match model.unwrap_or("") {
            m if m.contains("opus") => &OPUS,
            m if m.contains("haiku") => &HAIKU,
            _ => &SONNET,
        }
    }

    fn normalize_model<'a>(&self, model: &'a str) -> Cow<'a, str> {
        // "claude-opus-4-6-20251205" → "opus-4-6"
        let s = model.strip_prefix("claude-").unwrap_or(model);
        if let Some(idx) = s.rfind('-') {
            let tail = &s[idx + 1..];
            if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
                return Cow::Owned(s[..idx].to_string());
            }
        }
        Cow::Borrowed(s)
    }

    // Claude Code emits `<synthetic>` as a pseudo-model for compaction /
    // summary API calls — they don't correspond to billable tokens, so
    // drop them before aggregation. The trait default is a no-op, so
    // new providers don't inherit this Anthropic-specific filter.
    fn skip_model(&self, model: &str) -> bool {
        model == "<synthetic>"
    }
}

// Build candidate names to probe against LiteLLM's keyspace. LiteLLM
// tends to store Claude models under several forms; try the raw name
// first, then with common prefixes, then with the date suffix stripped.
fn claude_name_candidates(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(6);
    out.push(name.to_string());
    out.push(format!("anthropic/{name}"));
    // Strip trailing -YYYYMMDD if present.
    if let Some(idx) = name.rfind('-') {
        let tail = &name[idx + 1..];
        if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
            let base = &name[..idx];
            out.push(base.to_string());
            out.push(format!("anthropic/{base}"));
        }
    }
    out
}

// Anthropic pricing, per-million tokens. These are the hardcoded fallback
// used when `prices.json` (from `ccaudit refresh-prices`) isn't present.
// Numbers mirror what LiteLLM currently reports for Claude 4.x — the
// standard 5-minute cache write tier (1.25× input) and 90% cache-read
// discount. Values here are verified against:
//   https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json
// (keys: claude-opus-4-7, claude-sonnet-4-6, claude-haiku-4-5).
const OPUS: Pricing = Pricing {
    input: 5.0,
    output: 25.0,
    cache_write: 6.25,
    cache_read: 0.50,
};
const SONNET: Pricing = Pricing {
    input: 3.0,
    output: 15.0,
    cache_write: 3.75,
    cache_read: 0.30,
};
const HAIKU: Pricing = Pricing {
    input: 1.0,
    output: 5.0,
    cache_write: 1.25,
    cache_read: 0.10,
};

// ── Claude Code-specific project name prettifier ──

// Logs live in `~/.claude/projects/-Users-<username>-<rest>/`. We drop
// the prefix to get "code/ccaudit" style names for display.
pub fn prettify_project_name(raw: &str) -> String {
    let parts: Vec<&str> = raw.split('-').filter(|s| !s.is_empty()).collect();
    if parts.len() > 2 && parts.first().copied() == Some("Users") {
        return parts.get(2..).map_or_else(String::new, |s| s.join("/"));
    }
    raw.to_string()
}

fn project_name_for(path: &Path) -> Option<String> {
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(prettify_project_name)
        .filter(|s| !s.is_empty())
}

fn session_id_for(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn display_name_of(session: &Session) -> String {
    let raw = if let Some(s) = &session.summary {
        s.as_str()
    } else if let Some(m) = &session.first_user_msg {
        m.as_str()
    } else {
        session.id.as_str()
    };
    // Strip control chars at storage time so anything reading the raw
    // bytes from the cache (JSON output, future MCP server, etc.) gets
    // a clean string. Renderers also defensively re-sanitize.
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// Turn the Claude-shaped Session (which the TUI/web data model uses)
// into a provider-agnostic ParsedSession. Consecutive sub-messages with
// the same `message_id` are coalesced — the parser emits one per content
// block but they all represent a single API call.
fn to_parsed_session(path: &Path, src: &SourceFile, session: &Session) -> ParsedSession {
    let mut lines = Vec::new();
    let mut ts_unix = Vec::new();
    let mut last_id: Option<&str> = None;
    for msg in &session.messages {
        let Message {
            tokens,
            timestamp,
            message_id,
            model,
            ..
        } = msg;
        if !matches!(
            msg.kind,
            MessageKind::Assistant | MessageKind::ToolUse | MessageKind::Thinking
        ) {
            continue;
        }
        let Some(t) = tokens.as_ref() else { continue };
        let Some(ts) = *timestamp else { continue };
        let id = message_id.as_deref();
        if id.is_some() && id == last_id {
            continue;
        }
        last_id = id;
        lines.push(ParsedLine {
            day: day_from_ts(ts),
            msg_id_hash: id.map(|s| fnv1a(s.as_bytes())),
            model: model.clone(),
            input: t.input.min(u64::from(u32::MAX)) as u32,
            output: t.output.min(u64::from(u32::MAX)) as u32,
            cache_read: t.cache_read.min(u64::from(u32::MAX)) as u32,
            cache_create: t.cache_create.min(u64::from(u32::MAX)) as u32,
        });
        ts_unix.push(ts.timestamp());
    }
    ParsedSession {
        path_hash: src.path_hash,
        mtime: src.mtime,
        size: src.size,
        started_at: session.started_at,
        session_model: session.model.clone(),
        display_name: display_name_of(session),
        session_id: session_id_for(path),
        project_name: project_name_for(path),
        lines,
        ts_unix,
    }
}

// ── Scanners ──

// Bulk path (macOS only): one `getattrlistbulk(2)` per subdir batches
// all entries' (name, type, mtime, size) into a single kernel call.
// ~4× fewer syscalls than the portable path at this scale. Returns
// `None` on any FFI error so the caller can retry with default_scan.
#[cfg(target_os = "macos")]
fn scan_with_bulk(dir: &Path) -> Option<Vec<SourceFile>> {
    use super::bulk_scan_darwin::scan as bulk_scan;
    use super::path_hash;

    // Outer directory: we only need subdir names, so readdir + d_type is
    // already fine. Bulk-scanning the outer dir too would buy nothing.
    let Ok(entries) = fs::read_dir(dir) else {
        return Some(vec![]);
    };

    let mut out: Vec<SourceFile> = Vec::with_capacity(256);
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let subdir = e.path();
        // One syscall for all entries in this subdir.
        let items = bulk_scan(&subdir)?;
        for item in items {
            if !item.is_regular_file {
                continue;
            }
            if !Path::new(&item.name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
            {
                continue;
            }
            let p = subdir.join(&item.name);
            out.push(SourceFile {
                path_hash: path_hash(&p),
                path: p,
                mtime: item.mtime_secs,
                size: item.size,
            });
        }
    }
    Some(out)
}
