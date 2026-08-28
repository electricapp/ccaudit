// User configuration file.
//
// Everything here is optional and every field has a flag equivalent —
// the file exists so a preference (timezone, an archive location, a
// price the shipped table gets wrong) survives between invocations
// instead of being retyped. Flags always win over the file, so a config
// can never make a command line mean something other than what it says.
//
// Search order, first hit wins:
//   1. --config PATH
//   2. $CCAUDIT_CONFIG
//   3. ./ccaudit.json          (project-local, checked into a repo)
//   4. $XDG_CONFIG_HOME/ccaudit/config.json, else ~/.config/ccaudit/config.json
//
// A malformed file is an error, not a warning. Silently ignoring a
// typo'd price override would report wrong money and look right.

use crate::source::Pricing;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Ignored — present so editors can attach a schema without the
    /// `deny_unknown_fields` check rejecting the file.
    #[serde(rename = "$schema", default)]
    _schema: Option<String>,

    /// Default `--source`.
    #[serde(default)]
    pub source: Option<String>,
    /// Default `--timezone`.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Default `--locale`.
    #[serde(default)]
    pub locale: Option<String>,
    /// Default `--no-cost`.
    #[serde(default)]
    pub no_cost: Option<bool>,
    /// Default `--compact`.
    #[serde(default)]
    pub compact: Option<bool>,
    /// Default `--breakdown`.
    #[serde(default)]
    pub breakdown: Option<bool>,

    /// Key rebinds by action id (`down`, `open`, ...); values are key
    /// specs (`n`, `?`, `tab`, `ctrl-k`). Applies to the TUI and the web.
    #[serde(default)]
    pub keys: BTreeMap<String, String>,

    /// Per-provider settings, keyed by the `--source` id.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceConfig>,

    /// Per-model price overrides, keyed on the RAW model name as it
    /// appears in the logs (`claude-opus-4-6-20251205`), not the
    /// shortened display form. Matching on the raw name is what makes an
    /// override exact: display names collapse date suffixes, so
    /// `opus-4-6` would silently cover several distinct priced models.
    #[serde(default)]
    pub prices: BTreeMap<String, PriceConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Replaces this provider's default log location. Several roots read
    /// as one corpus, so a live directory and an archive can be reported
    /// together.
    #[serde(default)]
    pub logs_dirs: Vec<String>,
}

/// Per-million-token rates for one model.
///
/// Every base rate is required: a partial override would silently blend
/// user intent with a shipped default, and a missing `output` reading as
/// $0 is exactly the quiet wrong number this file exists to fix.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceConfig {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    /// 1-hour cache-write tier. Defaults to `cache_write` for models
    /// that only have the one tier.
    #[serde(default)]
    pub cache_write_1h: Option<f64>,
    pub cache_read: f64,
}

impl PriceConfig {
    fn to_pricing(&self) -> Pricing {
        Pricing {
            input: self.input,
            output: self.output,
            cache_write: self.cache_write,
            cache_write_1h: self.cache_write_1h.unwrap_or(self.cache_write),
            cache_read: self.cache_read,
        }
    }
}

/// Where a config would be looked for, in order. Exposed so `--help`
/// and error messages can name real paths rather than a description.
pub fn search_paths(explicit: Option<&str>) -> Vec<PathBuf> {
    if let Some(p) = explicit {
        return vec![PathBuf::from(p)];
    }
    let mut out = Vec::with_capacity(3);
    if let Some(p) = std::env::var_os("CCAUDIT_CONFIG") {
        out.push(PathBuf::from(p));
    }
    out.push(PathBuf::from("ccaudit.json"));
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")));
    if let Some(dir) = xdg {
        out.push(dir.join("ccaudit").join("config.json"));
    }
    out
}

/// Load the first config that exists. `Ok(None)` means none was found,
/// which is the normal case.
///
/// An `explicit` path that doesn't exist is an error — the user named a
/// file, and silently falling back to defaults would hide the typo.
pub fn load(explicit: Option<&str>) -> Result<Option<(PathBuf, Config)>, String> {
    for path in search_paths(explicit) {
        if !path.is_file() {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        let cfg: Config = serde_json::from_str(&body)
            .map_err(|e| format!("invalid config {}: {e}", path.display()))?;
        return Ok(Some((path, cfg)));
    }
    if let Some(p) = explicit {
        return Err(format!("config file not found: {p}"));
    }
    Ok(None)
}

impl Config {
    /// Log-root overrides in the shape `source::set_log_roots` wants.
    pub fn log_roots(&self) -> Vec<(String, Vec<PathBuf>)> {
        self.sources
            .iter()
            .filter(|(_, sc)| !sc.logs_dirs.is_empty())
            .map(|(id, sc)| (id.clone(), sc.logs_dirs.iter().map(PathBuf::from).collect()))
            .collect()
    }

    /// Price overrides in the shape `source::set_price_overrides` wants.
    pub fn price_overrides(&self) -> Vec<(String, Pricing)> {
        self.prices
            .iter()
            .map(|(model, p)| (model.clone(), p.to_pricing()))
            .collect()
    }
}

// Cache invalidation lives in `source::cache_path`, which folds
// `source::price_override_digest` and the configured roots into the
// cache filename. Costs are baked in at build time, so a changed rate
// has to land in a differently-named `.db` or the next run reports the
// old money from a cache that still looks fresh.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config, String> {
        serde_json::from_str(s).map_err(|e| e.to_string())
    }

    #[test]
    fn empty_object_is_a_valid_config() {
        let c = parse("{}").expect("empty config");
        assert!(c.prices.is_empty());
        assert!(c.sources.is_empty());
        assert_eq!(c.source, None);
    }

    #[test]
    fn schema_key_is_accepted() {
        // Editors add `$schema` for autocomplete; `deny_unknown_fields`
        // would otherwise reject the file it is meant to help write.
        let c = parse(r#"{"$schema":"https://example.com/s.json","source":"codex"}"#)
            .expect("config with $schema");
        assert_eq!(c.source.as_deref(), Some("codex"));
    }

    #[test]
    fn a_typo_is_rejected_rather_than_ignored() {
        // A silently-dropped `pricess` block would report the shipped
        // prices while the user believes their override applied.
        let err = parse(r#"{"pricess":{}}"#).unwrap_err();
        assert!(err.contains("unknown field"), "got: {err}");
    }

    #[test]
    fn a_partial_price_override_is_rejected() {
        let err = parse(r#"{"prices":{"m":{"input":1.0}}}"#).unwrap_err();
        assert!(err.contains("missing field"), "got: {err}");
    }

    #[test]
    fn one_cache_tier_defaults_to_the_write_rate() {
        let c = parse(
            r#"{"prices":{"m":{"input":1.0,"output":2.0,"cache_write":3.0,"cache_read":4.0}}}"#,
        )
        .expect("config");
        let p = c.prices.get("m").expect("m").to_pricing();
        assert_eq!(p.cache_write_1h, 3.0);
    }

    #[test]
    fn price_overrides_carry_every_column_through() {
        let c = parse(
            r#"{"prices":{"m":{"input":1.0,"output":2.0,"cache_write":3.0,"cache_write_1h":9.0,"cache_read":4.0}}}"#,
        )
        .expect("config");
        let overrides = c.price_overrides();
        assert_eq!(overrides.len(), 1);
        let (name, p) = overrides.first().expect("one override");
        assert_eq!(name, "m");
        assert_eq!((p.input, p.output), (1.0, 2.0));
        assert_eq!((p.cache_write, p.cache_write_1h), (3.0, 9.0));
        assert_eq!(p.cache_read, 4.0);
    }

    #[test]
    fn log_roots_skip_sources_that_configure_nothing() {
        // An empty list must read as "say nothing about this provider",
        // not as "scan no directories at all".
        let c =
            parse(r#"{"sources":{"codex":{"logs_dirs":[]},"claude-code":{"logs_dirs":["y"]}}}"#)
                .expect("config");
        let roots = c.log_roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots.first().expect("one root").0, "claude-code");
    }
}
