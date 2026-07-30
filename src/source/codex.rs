// OpenAI Codex CLI `Source` implementation.
//
// Codex writes session rollouts to
// `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`. Each line is a
// `RolloutLine { timestamp, type, payload }` envelope; the variants we care
// about are `session_meta`, `turn_context`, `response_item`, and the
// `event_msg` whose inner `payload.type == "token_count"` carries usage.

use super::{
    ParsedLine, ParsedSession, Pricing, Source, SourceFile, day_from_ts, fnv1a, path_hash,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Codex;

impl Source for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn logs_dir(&self) -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
    }

    fn scan_sources(&self) -> Vec<SourceFile> {
        let mut out: Vec<SourceFile> = Vec::with_capacity(128);
        for root in self.log_roots() {
            scan_yyyy_mm_dd(&root, &mut out);
        }
        out
    }

    fn parse_session(&self, src: &SourceFile) -> Option<ParsedSession> {
        parse_codex_session(src)
    }

    fn parse_messages(&self, path: &Path) -> Option<crate::parse::Session> {
        parse_codex_messages(path)
    }

    fn project_key(&self, _path: &Path, cwd: Option<&str>) -> String {
        // Rollouts live under `~/.codex/sessions/YYYY/MM/DD/`, so the
        // directory records when a session ran, not what it was about.
        // The `session_meta` cwd is the only real project signal;
        // sessions missing one share a single bucket rather than
        // fragmenting into one project per calendar day.
        cwd.unwrap_or("unknown").to_owned()
    }

    fn base_price(&self, model: Option<&str>) -> &Pricing {
        // `model` first so `price(None)` never forces the LiteLLM table
        // load — see the matching note in `ClaudeCode::price`.
        if let Some(name) = model {
            if let Some(lookup) = super::prices::get() {
                let candidates = openai_name_candidates(name);
                if let Some(p) = lookup.lookup(&candidates) {
                    return p;
                }
            }
        }
        // Hardcoded fallback (April 2026 OpenAI list prices). Refresh via
        // `ccaudit refresh-prices` to pick up LiteLLM rates (prices.json is
        // shared across providers).
        match model.unwrap_or("") {
            m if m.contains("mini") => &GPT5_MINI,
            m if m.contains("nano") => &GPT5_NANO,
            _ => &GPT5,
        }
    }

    fn normalize_model<'a>(&self, model: &'a str) -> Cow<'a, str> {
        // OpenAI model IDs ("gpt-5.4", "o3-mini") are already short — no
        // vendor prefix or date suffix to strip.
        Cow::Borrowed(model)
    }
}

// LiteLLM keys OpenAI models both bare and with an `openai/` prefix; try both.
fn openai_name_candidates(name: &str) -> [String; 2] {
    [name.to_string(), format!("openai/{name}")]
}

// OpenAI doesn't bill cache writes separately — the input rate covers
// them — and has no second cache TTL, so both write tiers are the input
// rate here.
const GPT5: Pricing = Pricing {
    input: 1.25,
    output: 10.0,
    cache_write: 1.25,
    cache_write_1h: 1.25,
    cache_read: 0.125,
};
const GPT5_MINI: Pricing = Pricing {
    input: 0.25,
    output: 2.0,
    cache_write: 0.25,
    cache_write_1h: 0.25,
    cache_read: 0.025,
};
const GPT5_NANO: Pricing = Pricing {
    input: 0.05,
    output: 0.40,
    cache_write: 0.05,
    cache_write_1h: 0.05,
    cache_read: 0.005,
};

// ── Scanner ──

// Three-level walk for `<root>/YYYY/MM/DD/*.jsonl`. Avoids generic recursion
// since the layout is fixed and shallow.
fn scan_yyyy_mm_dd(root: &Path, out: &mut Vec<SourceFile>) {
    let Ok(years) = fs::read_dir(root) else {
        return;
    };
    for y in years.flatten() {
        let Ok(months) = fs::read_dir(y.path()) else {
            continue;
        };
        for m in months.flatten() {
            let Ok(days) = fs::read_dir(m.path()) else {
                continue;
            };
            for d in days.flatten() {
                let Ok(files) = fs::read_dir(d.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let p = f.path();
                    if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Ok(meta) = f.metadata() else { continue };
                    if !meta.is_file() {
                        continue;
                    }
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs());
                    out.push(SourceFile {
                        path_hash: path_hash(&p),
                        path: p,
                        mtime,
                        size: meta.len(),
                    });
                }
            }
        }
    }
}

// ── Parser ──
//
// Minimal subset of Codex's RolloutItem schema. Each line's `payload`
// is captured as a raw, unparsed span and only deserialized for the
// line types we act on — a `#[serde(flatten)]` or tagged-enum shape
// here would force serde to buffer every line's full payload tree
// (large ignored `response_item` bodies included) into owned
// allocations before the tag dispatch could run. Unknown types skip
// their payload entirely, so a Codex CLI version bump that adds new
// ones doesn't break parsing.

#[derive(Deserialize)]
struct RolloutLine<'a> {
    timestamp: DateTime<Utc>,
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    payload: Option<&'a RawValue>,
}

fn payload_as<T: serde::de::DeserializeOwned>(payload: Option<&RawValue>) -> Option<T> {
    payload.and_then(|r| serde_json::from_str(r.get()).ok())
}

#[derive(Deserialize)]
struct SessionMetaPayload {
    id: String,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct TurnContextPayload {
    model: Option<String>,
}

#[derive(Deserialize)]
struct ResponseItemPayload {
    role: Option<String>,
    content: Option<Vec<ResponseContent>>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseContent {
    InputText {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventMsgPayload {
    TokenCount {
        info: Option<TokenUsageInfo>,
    },
    UserMessage {
        message: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct TokenUsageInfo {
    last_token_usage: TokenUsage,
}

#[derive(Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
}

/// Identity of one `last_token_usage` triple.
///
/// Codex re-emits an unchanged triple on rate-limit-only updates
/// (upstream #14489). Both passes over a rollout — the cache one below
/// and the transcript one further down — must skip exactly the same
/// events, or the browser's hour histogram would out-count the usage
/// report built from the cache. Sharing the hash is what guarantees it.
fn usage_hash(uncached: u64, cached: u64, output: u64) -> u64 {
    let mut buf = [0u8; 24];
    buf[0..8].copy_from_slice(&uncached.to_le_bytes());
    buf[8..16].copy_from_slice(&cached.to_le_bytes());
    buf[16..24].copy_from_slice(&output.to_le_bytes());
    fnv1a(&buf)
}

fn parse_codex_session(src: &SourceFile) -> Option<ParsedSession> {
    // Slurp the whole file — Codex sessions are small (typically <1 MB
    // even for long runs) so a single read beats line-by-line BufReader
    // (no per-line String alloc, fewer syscalls).
    let data = fs::read(&src.path).ok()?;

    let mut session_id = src
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut cwd: Option<String> = None;
    let mut current_model: Option<String> = None;
    let mut session_model: Option<String> = None;
    let mut first_user_msg: Option<String> = None;

    let mut lines: Vec<ParsedLine> = Vec::new();
    let mut ts_unix: Vec<i64> = Vec::new();
    // Codex re-emits an unchanged `last_token_usage` on rate-limit-only
    // updates (upstream issue #14489). Skip consecutive duplicates.
    let mut last_token_hash: Option<u64> = None;

    for raw in data.split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let Ok(line) = serde_json::from_slice::<RolloutLine>(raw) else {
            continue;
        };
        match line.kind.as_ref() {
            "session_meta" => {
                let Some(p) = payload_as::<SessionMetaPayload>(line.payload) else {
                    continue;
                };
                session_id = p.id;
                cwd = p.cwd;
                if started_at.is_none() {
                    started_at = Some(line.timestamp);
                }
            }
            "turn_context" => {
                let Some(p) = payload_as::<TurnContextPayload>(line.payload) else {
                    continue;
                };
                if let Some(m) = p.model {
                    if session_model.is_none() {
                        session_model = Some(m.clone());
                    }
                    // On a model switch, clear the consecutive-dup guard so
                    // an identical token triple under a *different* model
                    // isn't wrongly skipped.
                    if current_model.as_deref() != Some(m.as_str()) {
                        last_token_hash = None;
                    }
                    current_model = Some(m);
                }
            }
            // Once first_user_msg is set, response_item payloads (the
            // bulk of a rollout's bytes) are never deserialized at all.
            "response_item" if first_user_msg.is_none() => {
                let Some(p) = payload_as::<ResponseItemPayload>(line.payload) else {
                    continue;
                };
                if p.role.as_deref() != Some("user") {
                    continue;
                }
                if let Some(content) = p.content {
                    for c in content {
                        if let ResponseContent::InputText { text } = c {
                            if !text.is_empty() && !text.starts_with('<') {
                                first_user_msg = Some(text);
                                break;
                            }
                        }
                    }
                }
            }
            "event_msg" => match payload_as::<EventMsgPayload>(line.payload) {
                Some(EventMsgPayload::TokenCount { info: Some(info) }) => {
                    let u = info.last_token_usage;
                    let cached = u.cached_input_tokens.max(0) as u64;
                    // Codex `input_tokens` includes cached; subtract for the
                    // uncached-rate column.
                    let total_input = u.input_tokens.max(0) as u64;
                    let uncached = total_input.saturating_sub(cached);
                    let output = u.output_tokens.max(0) as u64;
                    // Local consecutive-dup guard ONLY: Codex re-emits an
                    // identical `last_token_usage` on rate-limit-only updates
                    // (upstream #14489), so skip a triple that exactly repeats
                    // the previous one under the same model. We deliberately
                    // emit `msg_id_hash: None` rather than hashing the triple
                    // into a global message id — Codex has no real message ids,
                    // and two genuinely distinct calls (other sessions, or
                    // non-consecutive in this one) can share a token triple;
                    // using it as a global dedup key silently undercounts them.
                    let h = usage_hash(uncached, cached, output);
                    if last_token_hash == Some(h) {
                        continue;
                    }
                    last_token_hash = Some(h);
                    lines.push(ParsedLine {
                        day: day_from_ts(line.timestamp),
                        msg_id_hash: None,
                        model: current_model.clone(),
                        input: uncached.min(u64::from(u32::MAX)) as u32,
                        output: output.min(u64::from(u32::MAX)) as u32,
                        cache_read: cached.min(u64::from(u32::MAX)) as u32,
                        cache_create: 0,
                        cache_create_1h: 0,
                    });
                    ts_unix.push(line.timestamp.timestamp());
                }
                Some(EventMsgPayload::UserMessage { message: Some(m) })
                    if first_user_msg.is_none() && !m.is_empty() && !m.starts_with('<') =>
                {
                    first_user_msg = Some(m);
                }
                _ => {}
            },
            _ => {}
        }
    }

    let display_name = first_user_msg
        .as_deref()
        .map(super::sanitize_control)
        .unwrap_or_else(|| session_id.clone());

    let project_name = cwd.as_deref().map(super::prettify_cwd);

    // Fall back to the first billable line's timestamp when the
    // `session_meta` line is missing/truncated, so the cache doesn't sort
    // this session to `i64::MIN` (the chronological front) at build time.
    let started_at = started_at.or_else(|| {
        ts_unix
            .first()
            .and_then(|&t| DateTime::from_timestamp(t, 0))
    });

    Some(ParsedSession {
        path_hash: src.path_hash,
        mtime: src.mtime,
        size: src.size,
        started_at,
        session_model,
        display_name,
        session_id,
        project_name,
        lines,
        ts_unix,
    })
}

// ── Transcript parsing (session browser) ──
//
// `parse_codex_session` above feeds the aggregation cache and reads only
// what the token columns need. The TUI and web transcript views want the
// conversation itself, so this second pass maps Codex's `response_item`
// payloads onto the canonical `parse::Message` shape.
//
// Only `response_item` lines become messages. Codex also emits an
// `event_msg`/`user_message` echo of every user turn, and keeping both
// would print each prompt twice — `response_item` is the model-visible
// conversation, so it wins. `event_msg` is read here only for
// `token_count`, which attaches usage to the call it followed.

/// One `response_item` payload, deserialized loosely.
///
/// Every field is optional and an unrecognized `type` falls through to a
/// skip: the Responses API item set grows with each Codex release, and a
/// variant we don't know yet has to degrade to "not rendered" rather
/// than failing the line (and with it the rest of the transcript).
#[derive(Deserialize)]
struct ItemPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    role: Option<String>,
    content: Option<Vec<ContentPart>>,
    /// `reasoning` items carry their visible text here, not in `content`.
    summary: Option<Vec<ContentPart>>,
    name: Option<String>,
    arguments: Option<String>,
    input: Option<String>,
    output: Option<serde_json::Value>,
    action: Option<serde_json::Value>,
}

// Matched on the presence of `text` rather than on the part's `type`:
// `input_text`, `output_text`, `summary_text` and bare `text` all carry
// it, while parts that don't (refusals, images) have nothing to render.
#[derive(Deserialize)]
struct ContentPart {
    text: Option<String>,
}

fn join_text(parts: Option<&[ContentPart]>) -> String {
    let mut out = String::new();
    for p in parts.unwrap_or_default() {
        let Some(t) = p.text.as_deref() else { continue };
        if t.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(t);
    }
    out
}

/// `function_call_output.output` is a bare JSON string in some Codex
/// versions and `{"content": "…", "metadata": {…}}` in others. Render
/// whichever is present, falling back to the raw JSON so a third shape
/// still shows something instead of an empty result block.
fn render_output(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| v.to_string(), str::to_string),
        other => other.to_string(),
    }
}

/// `local_shell_call.action` is `{"type":"exec","command":["bash","-lc","…"]}`.
/// Join the argv back into the command line the user would recognize.
fn render_action(v: &serde_json::Value) -> String {
    v.get("command")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| v.to_string())
}

/// Map one `response_item` onto a `Message` and append it.
///
/// Returns without pushing for item types the browser has nothing to
/// show for — that is the graceful-degradation path for future variants.
fn push_item(
    s: &mut crate::parse::Session,
    ts: Option<DateTime<Utc>>,
    model: Option<&str>,
    p: &ItemPayload,
) {
    use crate::parse::{Message, MessageKind};

    let (kind, content, tool_name) = match p.kind.as_deref().unwrap_or("") {
        "message" => {
            let text = join_text(p.content.as_deref());
            match p.role.as_deref().unwrap_or("") {
                "assistant" => (MessageKind::Assistant, text, None),
                // Codex injects environment context, permission blocks
                // and abort notices as `role: "user"` items wrapped in an
                // XML-ish tag. They're scaffolding, not something the
                // user typed, so they render as System and stay out of
                // both the turn count and the session title.
                "user" if !text.starts_with('<') => (MessageKind::User, text, None),
                _ => (MessageKind::System, text, None),
            }
        }
        "reasoning" => {
            let mut text = join_text(p.summary.as_deref());
            if text.is_empty() {
                text = join_text(p.content.as_deref());
            }
            (MessageKind::Thinking, text, None)
        }
        "function_call" | "custom_tool_call" => {
            let body = p
                .arguments
                .clone()
                .or_else(|| p.input.clone())
                .unwrap_or_default();
            (
                MessageKind::ToolUse,
                body,
                Some(p.name.clone().unwrap_or_else(|| "tool".to_owned())),
            )
        }
        "local_shell_call" => (
            MessageKind::ToolUse,
            p.action.as_ref().map(render_action).unwrap_or_default(),
            Some("shell".to_owned()),
        ),
        "function_call_output" | "custom_tool_call_output" => (
            MessageKind::ToolResult,
            p.output.as_ref().map(render_output).unwrap_or_default(),
            None,
        ),
        "web_search_call" => (
            MessageKind::ToolUse,
            String::new(),
            Some("web_search".to_owned()),
        ),
        _ => return,
    };

    // Same keep-rule as the Claude parser: tool calls survive an empty
    // rendered body (an unmodeled tool still happened), everything else
    // with no text is noise.
    if content.is_empty() && kind != MessageKind::ToolUse {
        return;
    }
    if kind == MessageKind::User {
        s.turn_count += 1;
        if s.first_user_msg.is_none() {
            s.first_user_msg = Some(crate::parse::truncated_copy(&content, 200));
        }
    }
    s.messages.push(Message {
        timestamp: ts,
        kind,
        content,
        tokens: None,
        tool_name,
        model: model.map(str::to_owned),
        message_id: None,
    });
}

fn parse_codex_messages(path: &Path) -> Option<crate::parse::Session> {
    use crate::parse::{Message, MessageKind, Session, TokenUsage};

    // Fingerprint BEFORE the read: an append racing the parse must leave
    // the per-session cache entry stale, never stamp a fresh
    // (mtime, size) onto content that predates it.
    let fingerprint = crate::parse::file_fingerprint(path)?;
    let data = fs::read(path).ok()?;

    let mut s = Session {
        id: path
            .file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or("unknown")
            .to_owned(),
        file_path: path.to_path_buf(),
        fingerprint: Some(fingerprint),
        ..Session::default()
    };
    let mut current_model: Option<String> = None;
    let mut last_token_hash: Option<u64> = None;

    for raw in data.split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let Ok(line) = serde_json::from_slice::<RolloutLine>(raw) else {
            continue;
        };
        let ts = Some(line.timestamp);
        match line.kind.as_ref() {
            "session_meta" => {
                if let Some(p) = payload_as::<SessionMetaPayload>(line.payload) {
                    s.id = p.id;
                    s.cwd = p.cwd;
                }
                if s.started_at.is_none() {
                    s.started_at = ts;
                }
            }
            "turn_context" => {
                let Some(m) = payload_as::<TurnContextPayload>(line.payload).and_then(|p| p.model)
                else {
                    continue;
                };
                if s.model.is_none() {
                    s.model = Some(m.clone());
                }
                // Mirrors the cache pass: on a model switch, clear the
                // consecutive-duplicate guard so an identical triple
                // under a *different* model isn't wrongly skipped.
                if current_model.as_deref() != Some(m.as_str()) {
                    last_token_hash = None;
                }
                current_model = Some(m);
            }
            "response_item" => {
                if let Some(p) = payload_as::<ItemPayload>(line.payload) {
                    push_item(&mut s, ts, current_model.as_deref(), &p);
                }
            }
            "event_msg" => {
                let Some(EventMsgPayload::TokenCount { info: Some(info) }) =
                    payload_as::<EventMsgPayload>(line.payload)
                else {
                    continue;
                };
                let u = info.last_token_usage;
                let cached = u.cached_input_tokens.max(0) as u64;
                // Codex `input_tokens` includes cached; subtract for the
                // uncached-rate column, exactly as the cache pass does.
                let uncached = (u.input_tokens.max(0) as u64).saturating_sub(cached);
                let output = u.output_tokens.max(0) as u64;
                let h = usage_hash(uncached, cached, output);
                if last_token_hash == Some(h) {
                    continue;
                }
                last_token_hash = Some(h);
                let usage = TokenUsage {
                    input: uncached,
                    output,
                    cache_read: cached,
                    // OpenAI bills cache writes at the input rate rather
                    // than as a separate column, and has no second cache
                    // TTL — see `GPT5`.
                    cache_create: 0,
                    cache_create_1h: 0,
                };
                // The count follows the API call it belongs to, so the
                // tokens land on whatever that call just produced: the
                // assistant message, or the `function_call` when the
                // model went straight to a tool. Attaching to the tail
                // instead of searching back for an assistant turn keeps
                // multi-call tool loops (one count per call) on the
                // right timestamps.
                if matches!(s.messages.last(), Some(m) if m.tokens.is_none()) {
                    if let Some(m) = s.messages.last_mut() {
                        m.tokens = Some(usage);
                    }
                } else {
                    s.messages.push(Message {
                        timestamp: ts,
                        kind: MessageKind::Assistant,
                        content: String::new(),
                        tokens: Some(usage),
                        tool_name: None,
                        model: current_model.clone(),
                        message_id: None,
                    });
                }
            }
            _ => {}
        }
    }

    s.msg_count = u32::try_from(s.messages.len()).unwrap_or(u32::MAX);
    // A truncated or missing `session_meta` would otherwise leave this
    // `None`, sorting the session to the front of every list.
    if s.started_at.is_none() {
        s.started_at = s.messages.first().and_then(|m| m.timestamp);
    }
    Some(s)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    unused_qualifications
)]
mod tests {
    use super::*;

    #[test]
    fn prettify_cwd_strips_users_prefix() {
        assert_eq!(
            super::super::prettify_cwd("/Users/me/code/cclog"),
            "code/cclog"
        );
        assert_eq!(
            super::super::prettify_cwd("/home/me/code/cclog"),
            "code/cclog"
        );
        assert_eq!(
            super::super::prettify_cwd("/opt/work/proj"),
            "/opt/work/proj"
        );
    }

    #[test]
    fn parses_token_count_and_dedups_repeats() {
        // Build a tiny in-memory rollout file mirroring Codex's emitted shape.
        let dir = std::env::temp_dir().join(format!("ccaudit-codex-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rollout-test.jsonl");
        let body = r#"{"timestamp":"2026-04-21T22:07:55.744Z","type":"session_meta","payload":{"id":"abc-123","cwd":"/Users/me/code/cclog"}}
{"timestamp":"2026-04-21T22:07:55.745Z","type":"turn_context","payload":{"model":"gpt-5.4"}}
{"timestamp":"2026-04-21T22:07:55.746Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello world"}]}}
{"timestamp":"2026-04-21T22:08:02.245Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":200,"output_tokens":500,"reasoning_output_tokens":100,"total_tokens":1500}}}}
{"timestamp":"2026-04-21T22:08:03.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":200,"output_tokens":500,"reasoning_output_tokens":100,"total_tokens":1500}}}}
"#;
        fs::write(&path, body).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let src = SourceFile {
            path_hash: 1,
            path: path.clone(),
            mtime: 0,
            size: meta.len(),
        };
        let s = parse_codex_session(&src).expect("parse");
        assert_eq!(s.session_id, "abc-123");
        assert_eq!(s.project_name.as_deref(), Some("code/cclog"));
        assert_eq!(s.display_name, "hello world");
        assert_eq!(s.session_model.as_deref(), Some("gpt-5.4"));
        // Two identical token_count events → one ParsedLine after dedup.
        assert_eq!(s.lines.len(), 1);
        let line = &s.lines[0];
        assert_eq!(line.input, 800); // 1000 - 200 cached
        assert_eq!(line.cache_read, 200);
        assert_eq!(line.output, 500);
        assert_eq!(line.cache_create, 0);
        assert_eq!(line.model.as_deref(), Some("gpt-5.4"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    // Writes `body` to a uniquely-named rollout file and hands back the
    // path plus a cleanup guard, so parallel test threads don't collide
    // on a shared filename the way a bare pid would.
    fn rollout(tag: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccaudit-codex-msgs-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("rollout-{tag}.jsonl"));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn transcript_maps_every_response_item_kind() {
        let path = rollout(
            "kinds",
            r#"{"timestamp":"2026-04-21T22:07:55.744Z","type":"session_meta","payload":{"id":"abc-123","cwd":"/Users/me/code/cclog"}}
{"timestamp":"2026-04-21T22:07:55.745Z","type":"turn_context","payload":{"model":"gpt-5.4"}}
{"timestamp":"2026-04-21T22:07:55.746Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"count the files"}]}}
{"timestamp":"2026-04-21T22:07:56.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"list then count"}]}}
{"timestamp":"2026-04-21T22:07:57.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":\"ls\"}","call_id":"c1"}}
{"timestamp":"2026-04-21T22:07:58.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":{"content":"a.rs\nb.rs"}}}
{"timestamp":"2026-04-21T22:07:59.000Z","type":"response_item","payload":{"type":"local_shell_call","action":{"type":"exec","command":["bash","-lc","wc -l"]}}}
{"timestamp":"2026-04-21T22:08:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"two files"}]}}
{"timestamp":"2026-04-21T22:08:01.000Z","type":"response_item","payload":{"type":"some_future_item","content":[{"type":"output_text","text":"ignored"}]}}
"#,
        );
        let s = parse_codex_messages(&path).expect("parse");

        assert_eq!(s.id, "abc-123");
        assert_eq!(s.cwd.as_deref(), Some("/Users/me/code/cclog"));
        assert_eq!(s.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(s.first_user_msg.as_deref(), Some("count the files"));
        assert_eq!(s.turn_count, 1);

        let got: Vec<(String, &str, Option<&str>)> = s
            .messages
            .iter()
            .map(|m| {
                (
                    m.kind.to_string(),
                    m.content.as_str(),
                    m.tool_name.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("USER".to_owned(), "count the files", None),
                ("THNK".to_owned(), "list then count", None),
                ("TOOL".to_owned(), r#"{"command":"ls"}"#, Some("shell")),
                ("RSLT".to_owned(), "a.rs\nb.rs", None),
                ("TOOL".to_owned(), "bash -lc wc -l", Some("shell")),
                ("ASST".to_owned(), "two files", None),
            ],
            "an unrecognized item type must be skipped, not abort the transcript"
        );
        assert_eq!(s.msg_count, 6);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn transcript_treats_injected_user_items_as_system() {
        // Codex wraps environment context and abort notices as
        // `role: "user"` items. They must not become the session title
        // or inflate the turn count.
        let path = rollout(
            "injected",
            r#"{"timestamp":"2026-04-21T22:07:55.746Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>"}]}}
{"timestamp":"2026-04-21T22:07:56.746Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}
{"timestamp":"2026-04-21T22:07:57.746Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"real question"}]}}
"#,
        );
        let s = parse_codex_messages(&path).expect("parse");
        let kinds: Vec<String> = s.messages.iter().map(|m| m.kind.to_string()).collect();
        assert_eq!(kinds, vec!["SYS", "SYS", "USER"]);
        assert_eq!(s.turn_count, 1);
        assert_eq!(s.first_user_msg.as_deref(), Some("real question"));
        // `session_meta` was missing — started_at still has to come from
        // somewhere, or the session sorts to the front of every list.
        assert!(s.started_at.is_some());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn transcript_does_not_double_count_user_turns() {
        // `event_msg`/`user_message` echoes the `response_item` the model
        // saw. Rendering both would print every prompt twice.
        let path = rollout(
            "echo",
            r#"{"timestamp":"2026-04-21T22:07:55.746Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
{"timestamp":"2026-04-21T22:07:55.747Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}
"#,
        );
        let s = parse_codex_messages(&path).expect("parse");
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.turn_count, 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn transcript_usage_matches_the_cache_pass() {
        // Both passes read the same rollout; a token_count the cache
        // skips as a duplicate must be skipped here too, or the web's
        // hour histogram out-counts `ccaudit daily`.
        let body = r#"{"timestamp":"2026-04-21T22:07:55.744Z","type":"session_meta","payload":{"id":"s1","cwd":"/Users/me/code/x"}}
{"timestamp":"2026-04-21T22:07:55.745Z","type":"turn_context","payload":{"model":"gpt-5.4"}}
{"timestamp":"2026-04-21T22:07:56.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"ls","call_id":"c1"}}
{"timestamp":"2026-04-21T22:07:57.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":200,"output_tokens":50}}}}
{"timestamp":"2026-04-21T22:07:58.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}
{"timestamp":"2026-04-21T22:07:59.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":200,"output_tokens":50}}}}
{"timestamp":"2026-04-21T22:08:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":90}}}}
"#;
        let path = rollout("usage", body);
        let meta = fs::metadata(&path).unwrap();
        let cached = parse_codex_session(&SourceFile {
            path_hash: 1,
            path: path.clone(),
            mtime: 0,
            size: meta.len(),
        })
        .expect("cache pass");
        let s = parse_codex_messages(&path).expect("transcript pass");

        let sum = |acc: (u64, u64, u64), t: &crate::parse::TokenUsage| {
            (acc.0 + t.input, acc.1 + t.output, acc.2 + t.cache_read)
        };
        let from_msgs = s
            .messages
            .iter()
            .filter_map(|m| m.tokens.as_ref())
            .fold((0, 0, 0), sum);
        let from_cache = cached.lines.iter().fold((0u64, 0u64, 0u64), |a, l| {
            (
                a.0 + u64::from(l.input),
                a.1 + u64::from(l.output),
                a.2 + u64::from(l.cache_read),
            )
        });
        assert_eq!(from_msgs, from_cache);
        // Two distinct calls survive the dedup; the repeat in between
        // is dropped by both passes.
        assert_eq!(cached.lines.len(), 2);
        assert_eq!(from_msgs, (800 + 1500, 50 + 90, 200 + 500));

        // The first count landed on the tool call it paid for, not on a
        // synthesized assistant row.
        let tooluse = s
            .messages
            .iter()
            .find(|m| m.kind == crate::parse::MessageKind::ToolUse)
            .expect("tool call");
        assert_eq!(tooluse.tokens.map(|t| t.output), Some(50));
        let _ = fs::remove_file(&path);
    }
}
