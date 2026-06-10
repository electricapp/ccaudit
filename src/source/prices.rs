// LiteLLM pricing integration (optional, refresh-on-demand).
//
// ccusage fetches https://github.com/BerriAI/litellm/.../model_prices_and_context_window.json
// at runtime to keep model prices current. We do the same but on an
// explicit user command (`ccaudit refresh-prices`) — the file is then
// cached under ~/.claude/ccaudit-cache/prices.json.
//
// Providers (e.g. ClaudeCode) consult `lookup()` first at price() time;
// a miss falls back to their hardcoded rate table. Since preaggs are
// priced at cache-build time, `refresh-prices` also deletes every
// provider usage cache (claude-code.db, codex.db) to force a rebuild with
// fresh rates — prices.json is shared, so a stale rate would otherwise
// linger in whichever provider wasn't rebuilt.

use super::{Pricing, SourceKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

pub fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("ccaudit-cache").join("prices.json"))
}

// ── LiteLLM schema (subset we use) ──

#[derive(Debug, Deserialize)]
struct LiteLLMEntry {
    #[serde(default)]
    input_cost_per_token: Option<f64>,
    #[serde(default)]
    output_cost_per_token: Option<f64>,
    #[serde(default)]
    cache_creation_input_token_cost: Option<f64>,
    #[serde(default)]
    cache_read_input_token_cost: Option<f64>,
}

// ── In-memory lookup ──

pub struct PricesLookup {
    // Keyed by the raw LiteLLM model name. Provider impls decide how to
    // match (exact / prefix / substring).
    entries: HashMap<String, Pricing>,
    // Lowercase copy of each key for case-insensitive substring match
    // (mirrors ccusage's fallback step).
    lower_keys: Vec<(String, String)>, // (lower, original)
}

impl PricesLookup {
    /// Look up a model with a two-stage strategy:
    ///   1. Exact match against any provider-scoped candidate name.
    ///   2. Word-boundary substring match: the candidate name contains
    ///      the key as a segment delimited by `-` / `/` / `.` / `_` / `:`
    ///      (or string ends). Among multiple matches, pick the longest
    ///      key (most specific). Only this direction is allowed —
    ///      accepting "key contains name" would mean a lookup for
    ///      `gpt-5` returns the price for `gpt-5-mini`, since the
    ///      latter contains the former.
    pub fn lookup(&self, candidates: &[String]) -> Option<&Pricing> {
        for c in candidates {
            if let Some(p) = self.entries.get(c) {
                return Some(p);
            }
        }
        let name = candidates.first()?;
        let lower = name.to_ascii_lowercase();
        let mut best: Option<(usize, &str)> = None;
        for (k_lower, k) in &self.lower_keys {
            if contains_at_boundary(&lower, k_lower) {
                let len = k_lower.len();
                if best.is_none_or(|(b, _)| len > b) {
                    best = Some((len, k.as_str()));
                }
            }
        }
        best.and_then(|(_, k)| self.entries.get(k))
    }
}

// Substring match that only counts as a hit when the matched span is
// delimited by a model-name separator (or by the start/end of the
// haystack). Keeps `claude-opus-4` matching `anthropic/claude-opus-4`
// but stops `gpt-5` from matching `gpt-5-mini`.
fn contains_at_boundary(haystack: &str, needle: &str) -> bool {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() || hb.len() < nb.len() {
        return false;
    }
    let max = hb.len() - nb.len();
    let mut i = 0;
    while i <= max {
        if hb.get(i..i + nb.len()) == Some(nb) {
            let before_ok = i == 0 || hb.get(i - 1).copied().is_some_and(is_name_separator);
            let after = i + nb.len();
            let after_ok =
                after == hb.len() || hb.get(after).copied().is_some_and(is_name_separator);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

const fn is_name_separator(b: u8) -> bool {
    matches!(b, b'-' | b'/' | b'.' | b'_' | b':')
}

// One-time-per-process lazy load. Missing file → `None`, no error.
static LOADED: OnceLock<Option<PricesLookup>> = OnceLock::new();

pub fn get() -> Option<&'static PricesLookup> {
    LOADED.get_or_init(load).as_ref()
}

// A missing file is the normal "no refresh yet" case → silent None. A
// file that exists but won't parse is genuine corruption: warn (to
// stderr, so stdout pipes stay clean) rather than silently reverting
// every cost to the built-in fallback table with no explanation.
#[allow(clippy::print_stderr)]
fn load() -> Option<PricesLookup> {
    let path = cache_path()?;
    let bytes = std::fs::read(&path).ok()?;
    match parse(&bytes) {
        Ok(lk) => Some(lk),
        Err(e) => {
            eprintln!(
                "warning: ignoring corrupt prices cache at {} ({e}); using built-in rates. Run `ccaudit refresh-prices` to repair.",
                path.display()
            );
            None
        }
    }
}

fn parse(bytes: &[u8]) -> Result<PricesLookup, String> {
    let raw: HashMap<String, serde_json::Value> =
        serde_json::from_slice(bytes).map_err(|e| format!("parse prices.json: {e}"))?;

    let mut entries: HashMap<String, Pricing> = HashMap::with_capacity(raw.len());
    for (name, value) in raw {
        let Ok(e) = serde_json::from_value::<LiteLLMEntry>(value) else {
            continue;
        };
        // We need at least input + output to price anything meaningfully.
        let (Some(in_c), Some(out_c)) = (e.input_cost_per_token, e.output_cost_per_token) else {
            continue;
        };
        // LiteLLM values are per-token; our Pricing struct is per-million.
        let p = Pricing {
            input: in_c * 1_000_000.0,
            output: out_c * 1_000_000.0,
            cache_write: e
                .cache_creation_input_token_cost
                .map(|c| c * 1_000_000.0)
                .unwrap_or(in_c * 1_000_000.0 * 1.25), // LiteLLM convention when unset
            cache_read: e
                .cache_read_input_token_cost
                .map(|c| c * 1_000_000.0)
                .unwrap_or(in_c * 1_000_000.0 * 0.1),
        };
        let _ = entries.insert(name, p);
    }
    let lower_keys: Vec<(String, String)> = entries
        .keys()
        .map(|k| (k.to_ascii_lowercase(), k.clone()))
        .collect();
    Ok(PricesLookup {
        entries,
        lower_keys,
    })
}

// ── Refresh command ──

pub struct RefreshResult {
    pub model_count: usize,
    pub bytes_written: usize,
    pub cache_path: PathBuf,
    pub invalidated_usage_db: bool,
}

pub fn refresh() -> Result<RefreshResult, String> {
    let out_path = cache_path().ok_or("HOME not set")?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }

    let body = http_get(LITELLM_URL)?;

    // Sanity-validate before overwriting the cache file.
    let parsed = parse(body.as_bytes())?;
    let model_count = parsed.entries.len();

    let tmp = out_path.with_extension("json.tmp");
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &out_path).map_err(|e| format!("rename {}: {e}", out_path.display()))?;

    // prices.json is shared across providers, so invalidate EVERY provider
    // usage cache — not just one — otherwise the providers we didn't
    // rebuild keep reporting costs computed at their last build time.
    let mut invalidated = false;
    for kind in [SourceKind::ClaudeCode, SourceKind::Codex] {
        if let Some(p) = super::pick(kind).cache_path() {
            if std::fs::remove_file(&p).is_ok() {
                invalidated = true;
            }
        }
    }

    Ok(RefreshResult {
        model_count,
        bytes_written: body.len(),
        cache_path: out_path,
        invalidated_usage_db: invalidated,
    })
}

fn http_get(url: &str) -> Result<String, String> {
    // Zero Rust deps for HTTP — shell out to the curl that ships with
    // every major OS (macOS, all Linuxes, Windows 10+). This keeps the
    // minimal binary under 600KB instead of pulling in a TLS stack.
    let out = Command::new("curl")
        .args([
            "-fsSL", // fail on HTTP errors, silent, follow redirects
            "--max-time",
            "30",
            url,
        ])
        .output()
        .map_err(|e| {
            format!(
                "curl not found ({e}). Install curl or place prices.json manually at {}.",
                cache_path().map_or_else(
                    || "~/.claude/ccaudit-cache/prices.json".to_string(),
                    |p| p.display().to_string()
                )
            )
        })?;
    if !out.status.success() {
        return Err(format!(
            "curl failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("non-utf8 response: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unused_qualifications)]
mod tests {
    use super::*;

    fn mk_lookup(keys: &[&str]) -> PricesLookup {
        let mut entries: HashMap<String, Pricing> = HashMap::new();
        for (i, k) in keys.iter().enumerate() {
            // Distinct prices so a wrong match is easy to detect.
            let n = (i as f64) + 1.0;
            let _ = entries.insert(
                (*k).to_string(),
                Pricing {
                    input: n,
                    output: n,
                    cache_write: n,
                    cache_read: n,
                },
            );
        }
        let lower_keys = entries
            .keys()
            .map(|k| (k.to_ascii_lowercase(), k.clone()))
            .collect();
        PricesLookup {
            entries,
            lower_keys,
        }
    }

    #[test]
    fn gpt5_does_not_match_gpt5_mini() {
        // The old `contains` both-ways logic returned gpt-5-mini's price
        // for a lookup of `gpt-5`. Now the lookup must miss (no exact
        // entry, no boundary-aligned substring match).
        let lk = mk_lookup(&["gpt-5-mini"]);
        assert!(lk.lookup(&["gpt-5".to_string()]).is_none());
    }

    #[test]
    fn provider_prefixed_key_matches_via_candidate_list() {
        // LiteLLM keys models as `openai/gpt-5-mini` / `anthropic/<name>`.
        // The matching strategy expects callers to enumerate both the
        // bare and prefixed forms in `candidates`, so exact match — not
        // the substring fallback — handles this case.
        let lk = mk_lookup(&["openai/gpt-5-mini"]);
        let p = lk
            .lookup(&["gpt-5-mini".to_string(), "openai/gpt-5-mini".to_string()])
            .expect("provider-prefixed candidate should match");
        assert!((p.input - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn name_extending_key_matches_via_substring_fallback() {
        // Substring path: the looked-up name has the registered key as
        // a `-`-delimited prefix. e.g. a model called
        // `claude-opus-4-7-20251205` matches the bare key
        // `claude-opus-4-7`. Boundary-aligned, longest-key-wins, only
        // when `name.contains(key)` (never the reverse).
        let lk = mk_lookup(&["claude-opus-4-7"]);
        let p = lk
            .lookup(&["claude-opus-4-7-20251205".to_string()])
            .expect("name-extends-key substring should match");
        assert!((p.input - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn longest_key_wins_among_boundary_matches() {
        // Both keys are boundary-aligned substrings of the haystack;
        // prefer the more specific (longer) one.
        let lk = mk_lookup(&["opus", "claude-opus-4-7"]);
        let p = lk
            .lookup(&["anthropic/claude-opus-4-7-20251205".to_string()])
            .expect("should pick the longest matching key");
        // Key index 1 (`claude-opus-4-7`) → price 2.0.
        assert!((p.input - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn exact_match_short_circuits_fallback() {
        // If a candidate matches exactly, that's the answer even when a
        // longer substring would also be boundary-aligned.
        let lk = mk_lookup(&["gpt-5", "gpt-5-mini"]);
        let p = lk.lookup(&["gpt-5".to_string()]).expect("exact match");
        assert!((p.input - 1.0).abs() < f64::EPSILON);
    }
}
