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
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
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
    /// 1-hour cache-write tier. Present on models that offer the longer
    /// TTL (Anthropic 4.5+); absent everywhere else, where the single
    /// cache-write rate applies to every write.
    #[serde(default)]
    cache_creation_input_token_cost_above_1hr: Option<f64>,
    #[serde(default)]
    cache_read_input_token_cost: Option<f64>,
}

// ── In-memory lookup ──

pub struct PricesLookup {
    // Keyed by the raw LiteLLM model name. Provider impls decide how to
    // match (exact / prefix / substring).
    entries: HashMap<String, Pricing>,
    // Lowercased key + rate for the case-insensitive substring fallback.
    // Inlining Pricing (32 bytes, Copy) saves ~3k key clones at load and a
    // second hash lookup per fallback hit.
    lower_keys: Vec<(String, Pricing)>,
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
        let mut best: Option<(usize, &Pricing)> = None;
        for (k_lower, pricing) in &self.lower_keys {
            // Cheap reject before the boundary scan: a key longer than the
            // haystack can't be a substring of it.
            if k_lower.len() > lower.len() {
                continue;
            }
            if contains_at_boundary(&lower, k_lower) {
                let len = k_lower.len();
                if best.is_none_or(|(b, _)| len > b) {
                    best = Some((len, pricing));
                }
            }
        }
        best.map(|(_, p)| p)
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

/// Per-token → per-million, rounded to drop float noise
/// (`1e-07 * 1e6` is `0.09999999999999999`).
fn per_million(cost_per_token: f64) -> f64 {
    (cost_per_token * 1_000_000.0 * 1e6).round() / 1e6
}

/// The 1-hour cache-write rate, distrusting the feed when it can't be true.
///
/// `LiteLLM` carries a stale `6e-06` on several Claude 3.x entries — it
/// belongs to `claude-3-7-sonnet` and was copied to its siblings. On
/// `claude-3-opus` that's a 1-hour tier cheaper than the same model's
/// 5-minute tier ($6.00 vs $18.75); on `claude-3-haiku` it's 24× input.
///
/// Accept a published rate in the range a real tier occupies, else fall
/// back to Anthropic's 2× input. That fallback reproduces the
/// hand-verified rates this table replaced ($30.00 / $0.50), which is
/// why it's trusted over the feed.
fn resolve_1h_tier(published: Option<f64>, input: f64, cache_write: f64) -> f64 {
    // Absent means one cache tier, so the write rate covers every write.
    let Some(rate) = published else {
        return cache_write;
    };
    if rate >= cache_write && rate <= input * 4.0 {
        rate
    } else {
        cache_write.max(input * 2.0)
    }
}

fn parse(bytes: &[u8]) -> Result<PricesLookup, String> {
    // Capture each model's body as an unparsed span and deserialize only
    // the four cost fields. A `serde_json::Value` tree for all ~3k models
    // cost ~2 ms on every command that prices a line — worst on
    // `statusline`, which polls. RawValue borrows straight out of `bytes`.
    let raw: HashMap<&str, &RawValue> =
        serde_json::from_slice(bytes).map_err(|e| format!("parse prices.json: {e}"))?;

    let mut entries: HashMap<String, Pricing> = HashMap::with_capacity(raw.len());
    for (name, value) in raw {
        // Non-object values (LiteLLM's `sample_spec` stub) aren't rate
        // entries — skip rather than failing the whole file.
        let Ok(e) = serde_json::from_str::<LiteLLMEntry>(value.get()) else {
            continue;
        };
        // We need at least input + output to price anything meaningfully.
        let (Some(in_c), Some(out_c)) = (e.input_cost_per_token, e.output_cost_per_token) else {
            continue;
        };
        // LiteLLM values are per-token; our Pricing struct is per-million.
        let input = per_million(in_c);
        let cache_write = e
            .cache_creation_input_token_cost
            .map_or(input * 1.25, per_million); // LiteLLM convention when unset
        let p = Pricing {
            input,
            output: per_million(out_c),
            cache_write,
            cache_write_1h: resolve_1h_tier(
                e.cache_creation_input_token_cost_above_1hr.map(per_million),
                input,
                cache_write,
            ),
            cache_read: e
                .cache_read_input_token_cost
                .map_or(input * 0.1, per_million),
        };
        let _ = entries.insert(name.to_string(), p);
    }
    let lower_keys: Vec<(String, Pricing)> = entries
        .iter()
        .map(|(k, p)| (k.to_ascii_lowercase(), *p))
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

// ── Built-in table generation ──
//
// `refresh-prices` keeps a user's rates current; this keeps the shipped
// binary's current, so a fresh install prices today's models before
// anyone runs anything. Same fetch and same `parse()`, so the generated
// table is exactly what a refresh would have produced.

/// Which keys get baked in. The bare `claude-*` names are canonical; the
/// `anthropic/`, `vertex_ai/` and regional variants restate the same
/// rates under prefixes that never appear in a Claude Code log.
fn is_builtin_key(name: &str) -> bool {
    name.starts_with("claude-")
}

pub struct EmitResult {
    pub model_count: usize,
    pub out_path: PathBuf,
}

/// Fetch `LiteLLM` and rewrite the checked-in fallback table. A developer
/// command — it writes Rust source, so the path is explicit, not default.
pub fn emit_builtin_table(out_path: &Path) -> Result<EmitResult, String> {
    let body = http_get(LITELLM_URL)?;
    let parsed = parse(body.as_bytes())?;

    let mut rows: Vec<(&str, &Pricing)> = parsed
        .entries
        .iter()
        .filter(|(k, _)| is_builtin_key(k))
        .map(|(k, p)| (k.as_str(), p))
        .collect();
    if rows.is_empty() {
        return Err(
            "no claude-* models in the LiteLLM response; refusing to write an empty table"
                .to_string(),
        );
    }
    // The lookup binary-searches this. Keys are unique, so unstable sort
    // has no ties to reorder between runs.
    rows.sort_unstable_by_key(|(k, _)| *k);

    let mut out = String::with_capacity(rows.len() * 160 + 2048);
    out.push_str(HEADER);
    // rustfmt would explode each entry to ten lines, so a regenerated
    // table would never match a formatted one and `fmt --check` would
    // fight the generator. One line per model is also just readable.
    let _ = writeln!(out, "#[rustfmt::skip]");
    let _ = writeln!(out, "pub static BUILTIN: &[(&str, Pricing)] = &[");
    for (name, p) in &rows {
        let _ = writeln!(
            out,
            "    ({:?}, Pricing {{ input: {:?}, output: {:?}, cache_write: {:?}, cache_write_1h: {:?}, cache_read: {:?} }}),",
            name, p.input, p.output, p.cache_write, p.cache_write_1h, p.cache_read
        );
    }
    out.push_str("];\n");

    std::fs::write(out_path, out.as_bytes())
        .map_err(|e| format!("write {}: {e}", out_path.display()))?;

    Ok(EmitResult {
        model_count: rows.len(),
        out_path: out_path.to_path_buf(),
    })
}

const HEADER: &str = concat!(
    "// @generated by `ccaudit refresh-prices --emit-table src/source/price_table.rs`.\n",
    "// Do not edit by hand — rerun that command instead.\n",
    "//\n",
    "// Anthropic rates from LiteLLM, per million tokens. The offline\n",
    "// fallback: a user who ran `refresh-prices` gets their newer copy\n",
    "// first, and a model in neither is priced at zero and reported.\n",
    "//\n",
    "// Sorted by key — `claude_code::builtin_price` binary-searches it.\n",
    "\n",
    "use super::Pricing;\n",
    "\n",
);

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
                    cache_write_1h: n,
                    cache_read: n,
                },
            );
        }
        let lower_keys = entries
            .iter()
            .map(|(k, p)| (k.to_ascii_lowercase(), *p))
            .collect();
        PricesLookup {
            entries,
            lower_keys,
        }
    }

    #[test]
    fn implausible_1h_tier_is_rejected() {
        // claude-3-opus as LiteLLM actually publishes it: a 1-hour rate
        // below the model's own 5-minute rate. Impossible, so the
        // documented 2x-input tier wins.
        assert!((resolve_1h_tier(Some(6.0), 15.0, 18.75) - 30.0).abs() < f64::EPSILON);
        // claude-3-haiku, same stale value, 24x its input rate.
        assert!((resolve_1h_tier(Some(6.0), 0.25, 0.3) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn plausible_1h_tier_is_kept_verbatim() {
        // Every current model publishes exactly 2x input; the guard must
        // not "correct" a number that was right to begin with.
        assert!((resolve_1h_tier(Some(20.0), 10.0, 12.5) - 20.0).abs() < f64::EPSILON);
        assert!((resolve_1h_tier(Some(6.0), 3.0, 3.75) - 6.0).abs() < f64::EPSILON);
        // Absent → the model has one tier; the write rate covers it.
        assert!((resolve_1h_tier(None, 1.25, 1.25) - 1.25).abs() < f64::EPSILON);
    }

    #[test]
    fn per_million_has_no_float_tail() {
        // 1e-07/token is $0.10/M, not $0.09999999999999999/M.
        assert!((per_million(1e-07) - 0.1).abs() < f64::EPSILON);
        assert!((per_million(2e-07) - 0.2).abs() < f64::EPSILON);
        assert!((per_million(1.875e-05) - 18.75).abs() < f64::EPSILON);
    }

    #[test]
    fn gpt5_does_not_match_gpt5_mini() {
        // A both-ways `contains` match returns gpt-5-mini's price for a
        // lookup of `gpt-5`. The lookup must miss instead (no exact
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
