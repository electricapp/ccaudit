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
        let Some(root) = self.logs_dir() else {
            return vec![];
        };
        let mut out: Vec<SourceFile> = Vec::with_capacity(128);
        scan_yyyy_mm_dd(&root, &mut out);
        out
    }

    fn parse_session(&self, src: &SourceFile) -> Option<ParsedSession> {
        parse_codex_session(src)
    }

    fn price(&self, model: Option<&str>) -> &Pricing {
        if let Some(lookup) = super::prices::get() {
            if let Some(name) = model {
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
fn openai_name_candidates(name: &str) -> Vec<String> {
    vec![name.to_string(), format!("openai/{name}")]
}

const GPT5: Pricing = Pricing {
    input: 1.25,
    output: 10.0,
    // OpenAI doesn't bill cache writes separately — the input rate covers it.
    cache_write: 1.25,
    cache_read: 0.125,
};
const GPT5_MINI: Pricing = Pricing {
    input: 0.25,
    output: 2.0,
    cache_write: 0.25,
    cache_read: 0.025,
};
const GPT5_NANO: Pricing = Pricing {
    input: 0.05,
    output: 0.40,
    cache_write: 0.05,
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
// Minimal subset of Codex's RolloutItem schema. Each line is a typed
// `RolloutLine` whose `body` is a tag-discriminated enum, so serde
// parses the payload into the right variant in a single pass — no
// `serde_json::Value` re-walk per line. Unknown variants land in
// `Other` so a Codex CLI version bump that adds new ones doesn't break
// parsing.

#[derive(Deserialize)]
struct RolloutLine {
    timestamp: DateTime<Utc>,
    #[serde(flatten)]
    body: RolloutBody,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum RolloutBody {
    SessionMeta(SessionMetaPayload),
    TurnContext(TurnContextPayload),
    ResponseItem(ResponseItemPayload),
    EventMsg(EventMsgPayload),
    #[serde(other)]
    Other,
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
        match line.body {
            RolloutBody::SessionMeta(p) => {
                session_id = p.id;
                cwd = p.cwd;
                if started_at.is_none() {
                    started_at = Some(line.timestamp);
                }
            }
            RolloutBody::TurnContext(p) => {
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
            RolloutBody::ResponseItem(p) if first_user_msg.is_none() => {
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
            RolloutBody::EventMsg(EventMsgPayload::TokenCount { info: Some(info) }) => {
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
                let mut buf = [0u8; 24];
                buf[0..8].copy_from_slice(&uncached.to_le_bytes());
                buf[8..16].copy_from_slice(&cached.to_le_bytes());
                buf[16..24].copy_from_slice(&output.to_le_bytes());
                let h = fnv1a(&buf);
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
                });
                ts_unix.push(line.timestamp.timestamp());
            }
            RolloutBody::EventMsg(EventMsgPayload::UserMessage { message: Some(m) })
                if first_user_msg.is_none() && !m.is_empty() && !m.starts_with('<') =>
            {
                first_user_msg = Some(m);
            }
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
}
