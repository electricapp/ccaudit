// Claude Code `Source` implementation.
//
// Everything provider-specific about Anthropic's Claude Code logs lives
// here: where JSONL files are, how to parse them, how models are priced
// and named. Adding a new provider means adding a sibling of this file;
// no other layer needs to change.

use super::{ParsedLine, ParsedSession, Pricing, Source, SourceFile, day_from_ts, fnv1a};
use crate::parse::{self, Message, MessageKind, Session};
use std::borrow::Cow;
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
        // Cache-first: if a fresh per-session cache exists, skip the
        // JSONL parse entirely. Critical for `cache::load` invocations
        // where only the `.db` was wiped (e.g. after `refresh-prices`):
        // 300 .msgs reads beat 300 JSONL re-parses by a factor of ~10.
        if let Some(session) = parse::try_load_cached_full(&src.path) {
            return Some(to_parsed_session(&src.path, src, &session));
        }
        // allow_empty: a readable session with no billable lines must
        // still yield `Some` — see the trait contract. A `None` for a
        // file that keeps being scanned leaves the cache one session
        // short of the scan count, failing validation and forcing a
        // full rebuild on every run.
        let session = parse::parse_session_allow_empty(&src.path)?;
        // Persist so the matching `try_load_cached_header` in
        // `load_all_projects`'s par_iter (which runs right after
        // `cache::load`) finds a hot cache and skips its own parse —
        // otherwise the cold path parses every file twice.
        parse::save_session_to_cache(&src.path, &session);
        Some(to_parsed_session(&src.path, src, &session))
    }

    fn parse_messages(&self, path: &Path) -> Option<Session> {
        parse::parse_session_allow_empty(path)
    }

    fn project_key(&self, path: &Path, _cwd: Option<&str>) -> String {
        // Group by the project directory, not the file's immediate
        // parent: subagent transcripts live at
        // `<project>/<uuid>/subagents/agent-*.jsonl`, and bucketing
        // those by `parent()` would invent a `subagents` project per
        // conversation instead of folding them into the real one.
        project_root_of(path)
            .or_else(|| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| path.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    fn price(&self, model: Option<&str>) -> &Pricing {
        // 1. Try the user's refreshed LiteLLM cache (if present). This
        //    matches ccusage's approach and keeps prices current without
        //    a code change. Multiple name variants are tried (exact +
        //    `anthropic/` prefix + date-stripped form) to cover how
        //    LiteLLM tends to key Claude models.
        // `model` first: `price(None)` has nothing to look up, and
        // `prices::get()` would force the whole LiteLLM table to load just
        // to fall through. `ModelRates::build` calls it on every run.
        if let Some(name) = model {
            if let Some(lookup) = super::prices::get() {
                let (candidates, len) = claude_name_candidates(name);
                #[allow(clippy::indexing_slicing)]
                if let Some(p) = lookup.lookup(&candidates[..len]) {
                    return p;
                }
            }
        }
        // 2. Fall back to the hardcoded table (March 2026 prices).
        //    Claude 3.x rates differ sharply from 4.x (3-opus is 3× the
        //    4.x opus price), so match the generation before the family.
        match model.unwrap_or("") {
            m if m.contains("3-opus") => &OPUS_3,
            m if m.contains("3-5-haiku") => &HAIKU_3_5,
            m if m.contains("3-haiku") => &HAIKU_3,
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
fn claude_name_candidates(name: &str) -> ([String; 4], usize) {
    let mut out = [
        name.to_string(),
        format!("anthropic/{name}"),
        String::new(),
        String::new(),
    ];
    let mut len = 2;
    if let Some(idx) = name.rfind('-') {
        let tail = &name[idx + 1..];
        if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
            let base = &name[..idx];
            #[allow(clippy::indexing_slicing)]
            {
                if len < 4 {
                    out[len] = base.to_string();
                    len += 1;
                }
                if len < 4 {
                    out[len] = format!("anthropic/{base}");
                    len += 1;
                }
            }
        }
    }
    (out, len)
}

// Anthropic pricing, per-million tokens. These are the hardcoded fallback
// used when `prices.json` (from `ccaudit refresh-prices`) isn't present.
// Numbers mirror what LiteLLM currently reports for Claude 4.x: the
// 5-minute cache write tier at 1.25× input, the 1-hour tier at 2×, and a
// 90% cache-read discount. Values here are verified against:
//   https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json
// (keys: claude-opus-4-7, claude-sonnet-4-6, claude-haiku-4-5).
const OPUS: Pricing = Pricing {
    input: 5.0,
    output: 25.0,
    cache_write: 6.25,
    cache_write_1h: 10.0,
    cache_read: 0.50,
};
const SONNET: Pricing = Pricing {
    input: 3.0,
    output: 15.0,
    cache_write: 3.75,
    cache_write_1h: 6.0,
    cache_read: 0.30,
};
const HAIKU: Pricing = Pricing {
    input: 1.0,
    output: 5.0,
    cache_write: 1.25,
    cache_write_1h: 2.0,
    cache_read: 0.10,
};
// Claude 3.x generations (legacy logs). Keys: claude-3-opus,
// claude-3-5-haiku, claude-3-haiku in the same LiteLLM table.
const OPUS_3: Pricing = Pricing {
    input: 15.0,
    output: 75.0,
    cache_write: 18.75,
    cache_write_1h: 30.0,
    cache_read: 1.50,
};
const HAIKU_3_5: Pricing = Pricing {
    input: 0.80,
    output: 4.0,
    cache_write: 1.0,
    cache_write_1h: 1.6,
    cache_read: 0.08,
};
const HAIKU_3: Pricing = Pricing {
    input: 0.25,
    output: 1.25,
    cache_write: 0.30,
    cache_write_1h: 0.5,
    cache_read: 0.03,
};

// ── Claude Code-specific project name prettifier ──

// Logs live in `~/.claude/projects/-Users-<username>-<rest>/`. The dir
// name is dash-encoded and lossy when the real path contains hyphens
// (e.g. `-Users-me-code-foo-bar` could be `code/foo/bar` or
// `code/foo-bar`). Prefer the unambiguous `cwd` from the JSONL body
// when available; fall back to splitting the dir name on dashes.
pub fn prettify_project_name(raw: &str) -> String {
    let parts: Vec<&str> = raw.split('-').filter(|s| !s.is_empty()).collect();
    super::prettify_user_path(&parts).unwrap_or_else(|| raw.to_string())
}

/// The project dir a session file belongs to.
///
/// The direct child of `~/.claude/projects` containing `path`, however
/// deeply nested. `path.parent()` only works for the flat layout — a
/// subagent transcript's parent is `subagents`, which would otherwise be
/// read as the project name.
pub fn project_root_of(path: &Path) -> Option<PathBuf> {
    let root = logs_root()?;
    let first = path.strip_prefix(root).ok()?.components().next()?;
    Some(root.join(first))
}

// Runs once per scanned session; the logs root can't change mid-process.
fn logs_root() -> Option<&'static Path> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| ClaudeCode.logs_dir()).as_deref()
}

fn project_name_for(path: &Path, cwd: Option<&str>) -> Option<String> {
    if let Some(c) = cwd {
        let pretty = super::prettify_cwd(c);
        if !pretty.is_empty() {
            return Some(pretty);
        }
    }
    // Dash-encoded project dir name, resolved from the logs root so
    // nested sessions still land on their real project.
    project_root_of(path)
        .as_deref()
        .or_else(|| path.parent())
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

/// The conversation id `claude -r` can resume for a session file.
///
/// A flat `<project>/<uuid>.jsonl` resumes as `<uuid>`. A subagent
/// transcript isn't resumable itself — `claude` only knows its parent,
/// whose id is the directory below the project root. `None` for paths
/// outside the logs root; callers fall back to the file stem.
pub fn resume_target_of(path: &Path) -> Option<String> {
    let rel = path.strip_prefix(logs_root()?).ok()?;
    let mut comps = rel.components();
    let _project_dir = comps.next()?;
    let name = comps.next()?.as_os_str().to_str()?;
    if comps.next().is_none() {
        // Flat layout: this component is the session file itself.
        Some(name.strip_suffix(".jsonl").unwrap_or(name).to_string())
    } else {
        // Nested: `name` is the parent conversation's directory.
        Some(name.to_string())
    }
}

// Single source for the session-display-name fallback chain. `Session::display_name`
// (in `parse.rs`) and the JS `dn()` helper (in `web/util.js`) follow the same
// rule. Sanitize control chars here so the cache stores a clean string —
// renderers can still defensively re-escape.
fn display_name_of(session: &Session) -> String {
    super::sanitize_control(session.display_name())
}

// Turn the Claude-shaped Session (which the TUI/web data model uses)
// into a provider-agnostic ParsedSession. Consecutive sub-messages with
// the same `message_id` are coalesced — the parser emits one per content
// block, but they all describe a single API call.
//
// Coalescing keeps the LAST line of a run, not the first. Claude Code
// rewrites a streamed assistant message as it arrives, and `usage`
// carries the running total: `input_tokens` and the cache counts are
// fixed by the request and repeat unchanged, while `output_tokens` grows
// with each line. Keeping the first line therefore booked a partial
// output count for every streamed message — 4.4M tokens (~20% of all
// output) missing across a year of local logs.
fn to_parsed_session(path: &Path, src: &SourceFile, session: &Session) -> ParsedSession {
    let mut lines: Vec<ParsedLine> = Vec::new();
    let mut ts_unix: Vec<i64> = Vec::new();
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
        let line = ParsedLine {
            day: day_from_ts(ts),
            msg_id_hash: id.map(|s| fnv1a(s.as_bytes())),
            model: model.clone(),
            input: t.input.min(u64::from(u32::MAX)) as u32,
            output: t.output.min(u64::from(u32::MAX)) as u32,
            cache_read: t.cache_read.min(u64::from(u32::MAX)) as u32,
            cache_create: t.cache_create.min(u64::from(u32::MAX)) as u32,
            cache_create_1h: t.cache_create_1h.min(u64::from(u32::MAX)) as u32,
        };
        // Same id as the previous line: overwrite it rather than append,
        // so the run collapses to its final, complete usage. `max` on
        // each column rather than a blind overwrite, because a later
        // line reporting *less* than an earlier one would otherwise
        // discard tokens that were already billed.
        if id.is_some() && id == last_id {
            if let Some(prev) = lines.last_mut() {
                prev.output = prev.output.max(line.output);
                prev.input = prev.input.max(line.input);
                prev.cache_read = prev.cache_read.max(line.cache_read);
                prev.cache_create = prev.cache_create.max(line.cache_create);
                prev.cache_create_1h = prev.cache_create_1h.max(line.cache_create_1h);
                continue;
            }
        }
        last_id = id;
        lines.push(line);
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
        project_name: project_name_for(path, session.cwd.as_deref()),
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
    let mut out: Vec<SourceFile> = Vec::with_capacity(256);
    bulk_scan_recursive(dir, super::MAX_SCAN_DEPTH, &mut out)?;
    Some(out)
}

// Must match `default_scan`'s traversal: the cache validates by session
// count, so a path only one scanner sees flips the cache between valid
// and stale depending on CCAUDIT_BULK_SCAN. `None` on any FFI error so
// the caller retries the whole tree portably.
#[cfg(target_os = "macos")]
fn bulk_scan_recursive(dir: &Path, depth_left: usize, out: &mut Vec<SourceFile>) -> Option<()> {
    use super::bulk_scan_darwin::scan as bulk_scan;
    use super::path_hash;

    if depth_left == 0 {
        return Some(());
    }
    let items = bulk_scan(dir)?;
    for item in items {
        let p = dir.join(&item.name);
        if !item.is_regular_file {
            // getattrlistbulk reports the entry's own type, so a symlinked
            // dir arrives as non-regular; is_dir() follows links.
            if p.is_dir() {
                bulk_scan_recursive(&p, depth_left - 1, out)?;
            }
            continue;
        }
        // Exact, case-sensitive ".jsonl" — identical to default_scan,
        // so the bulk path can't include `FOO.JSONL` that the portable
        // path would skip (or vice-versa).
        if Path::new(&item.name).extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        out.push(SourceFile {
            path_hash: path_hash(&p),
            path: p,
            mtime: item.mtime_secs,
            size: item.size,
        });
    }
    Some(())
}
