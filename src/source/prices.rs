// LiteLLM pricing integration (optional, refresh-on-demand).
//
// ccusage fetches https://github.com/BerriAI/litellm/.../model_prices_and_context_window.json
// at runtime to keep model prices current. We do the same but on an
// explicit user command (`ccaudit refresh-prices`) — the file is then
// cached under ~/.claude/ccaudit-cache/prices.json.
//
// Providers (e.g. ClaudeCode) consult `lookup()` first at price() time;
// a miss falls back to their hardcoded rate table. Since preaggs are
// priced at cache-build time, `refresh-prices` also deletes `usage.db`
// to force a rebuild with fresh rates.

use super::{Pricing, Source};
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
    /// Look up a model with the same matching strategy ccusage uses:
    ///   1. Exact match against any provider-scoped candidate name
    ///   2. Case-insensitive substring match
    pub fn lookup(&self, candidates: &[String]) -> Option<&Pricing> {
        for c in candidates {
            if let Some(p) = self.entries.get(c) {
                return Some(p);
            }
        }
        if let Some(name) = candidates.first() {
            let lower = name.to_ascii_lowercase();
            for (k_lower, k) in &self.lower_keys {
                if k_lower.contains(&lower) || lower.contains(k_lower.as_str()) {
                    if let Some(p) = self.entries.get(k) {
                        return Some(p);
                    }
                }
            }
        }
        None
    }
}

// One-time-per-process lazy load. Missing file → `None`, no error.
static LOADED: OnceLock<Option<PricesLookup>> = OnceLock::new();

pub fn get() -> Option<&'static PricesLookup> {
    LOADED.get_or_init(load).as_ref()
}

fn load() -> Option<PricesLookup> {
    let path = cache_path()?;
    let bytes = std::fs::read(&path).ok()?;
    parse(&bytes).ok()
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

pub fn refresh(source: &dyn Source) -> Result<RefreshResult, String> {
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

    // Invalidate the active source's usage cache so the next `ccaudit`
    // run rebuilds preaggs with the new prices. Without this the
    // rollup numbers would still reflect the prices used at the last
    // build time.
    let invalidated = source
        .cache_path()
        .map(|p| std::fs::remove_file(&p).is_ok())
        .unwrap_or(false);

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
