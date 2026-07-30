// Source: per-provider log schema + pricing.
//
// Every upstream (Claude Code, Codex, Pi, OpenCode, Amp, …) writes JSONL
// in its own shape with its own model names and its own pricing. The
// `Source` trait hides all of that from the cache + aggregation + report
// layers so they stay provider-agnostic.

use chrono::{DateTime, NaiveDate, Utc};
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

pub mod claude_code;
pub mod codex;
pub mod prices;

#[cfg(target_os = "macos")]
pub mod bulk_scan_darwin;

// ── Canonical records ──
// These are what every layer above `source` sees. Providers produce
// them; cache/agg/report consume them.

pub struct ParsedSession {
    pub path_hash: u64,
    pub mtime: u64,
    pub size: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub session_model: Option<String>,
    pub display_name: String,
    pub session_id: String,
    /// Some("foo/bar") when the provider groups sessions by project
    /// (Claude Code derives this from the logs directory name); None
    /// for providers that don't have a project concept (Codex stores
    /// everything under a flat `~/.codex/sessions/`).
    pub project_name: Option<String>,
    pub lines: Vec<ParsedLine>,
    pub ts_unix: Vec<i64>,
}

pub struct ParsedLine {
    pub day: i32,
    pub msg_id_hash: Option<u64>,
    pub model: Option<String>,
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    /// Total cache-creation tokens, both TTLs.
    pub cache_create: u32,
    /// Portion of `cache_create` written at the 1-hour TTL. A subset of
    /// it, priced at a higher rate — see [`Tokens`].
    pub cache_create_1h: u32,
    // NOTE: when a provider (o1/o3-style reasoning models) starts emitting
    // reasoning-class tokens, add `pub reasoning: u32` here, mirror it in
    // Pricing + compute_cost, add a matching column to LineEntry / PreAgg
    // in `cache/schema.rs` (re-checking the size_of asserts), and bump
    // `VERSION`. Today every shipping provider maps cleanly into the four
    // columns above so the slot would just waste bytes.
}

#[derive(Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    pub path_hash: u64,
    pub mtime: u64,
    pub size: u64,
}

/// Token counts for one pricing call.
///
/// `cache_write_1h` is the portion of `cache_write` written at the
/// 1-hour TTL — a subset of it, not an amount on top. Anthropic bills
/// the two cache TTLs at different multiples of the input rate (1.25×
/// for five minutes, 2× for an hour), so one blended cache-write number
/// cannot price a session that used both.
#[derive(Clone, Copy, Default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
}

// Per-million-token prices. A provider returns one of these for each
// model it knows about; unknown models fall back to the provider's
// default (typically Sonnet-tier).
#[derive(Clone, Copy, Debug)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    /// 5-minute cache-write tier.
    pub cache_write: f64,
    /// 1-hour cache-write tier. Providers without a second TTL set this
    /// equal to `cache_write`.
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

impl Pricing {
    /// Per-column dollar cost for a token count. The single arithmetic
    /// primitive — `price_columns` and the memoized `ModelRates` both
    /// route through it so float-summation ordering stays identical
    /// across cache build, totals, and report output.
    ///
    /// Both cache tiers land in the same (third) column: they are one
    /// line item on the bill and one column in every report, priced at
    /// two rates.
    pub fn columns(&self, t: Tokens) -> [f64; 4] {
        // Clamp: a provider claiming more 1h tokens than total cache
        // writes is contradicting itself, and the subtraction below would
        // underflow.
        let long_ttl = t.cache_write_1h.min(t.cache_write);
        let short_ttl = t.cache_write - long_ttl;
        [
            (t.input as f64) * self.input / 1_000_000.0,
            (t.output as f64) * self.output / 1_000_000.0,
            (long_ttl as f64).mul_add(self.cache_write_1h, (short_ttl as f64) * self.cache_write)
                / 1_000_000.0,
            (t.cache_read as f64) * self.cache_read / 1_000_000.0,
        ]
    }
}

// ── Source trait ──
//
// The only thing a new provider writes. Everything else — cache, agg,
// report — calls through these methods. A minimal new provider is four
// methods (id, display_name, logs_dir, parse_session) + a price table;
// the rest inherit sensible defaults.
pub trait Source: Sync + Send {
    /// Stable short id, e.g. "claude-code", "codex". Used as the cache
    /// filename stem and as the `--source` flag value. Must match one
    /// of the aliases accepted by `SourceKind::from_str`.
    fn id(&self) -> &'static str;

    /// Human-friendly label shown in report titles and error messages,
    /// e.g. "Claude Code", "Codex", "`OpenCode`".
    fn display_name(&self) -> &'static str;

    /// Directory where this provider's logs live by default. `None` when
    /// the platform doesn't expose a home dir (rare). Providers whose
    /// sessions aren't filesystem-rooted (e.g. a SQLite-backed provider)
    /// return the containing directory of the db.
    ///
    /// Scanners read [`Source::log_roots`], not this — the user can point
    /// a provider at other directories entirely.
    fn logs_dir(&self) -> Option<PathBuf>;

    /// Every directory to scan for this provider's logs.
    ///
    /// A configured override replaces `logs_dir()` outright rather than
    /// adding to it: pointing at an archive should report that archive,
    /// not silently blend it with whatever is in `$HOME`.
    fn log_roots(&self) -> Vec<PathBuf> {
        match log_root_override(self.id()) {
            Some(roots) => roots.to_vec(),
            None => self.logs_dir().into_iter().collect(),
        }
    }

    /// Binary cache file path. Default: `{cache_root}/{id()}.db`, or
    /// `{cache_root}/{id()}-{hash}.db` when the roots are overridden —
    /// two corpora under one filename would invalidate each other on
    /// every alternating run.
    fn cache_path(&self) -> Option<PathBuf> {
        let mut h: u64 = price_override_digest();
        if let Some(roots) = log_root_override(self.id()) {
            for r in roots {
                h ^= path_hash(r);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        if h == 0 {
            return default_cache_path(self.id());
        }
        default_cache_path(&format!("{}-{h:016x}", self.id()))
    }

    /// Enumerate every session available from this provider without
    /// parsing. Default walks `logs_dir()` and yields every `*.jsonl`
    /// file. Providers with non-file layouts (`SQLite`, archive, etc.)
    /// override to synthesize one `SourceFile` per session, stashing
    /// whatever identity info they need in `path_hash` + `path`.
    fn scan_sources(&self) -> Vec<SourceFile> {
        let mut out = Vec::new();
        for root in self.log_roots() {
            out.extend(default_scan(&root));
        }
        out
    }

    /// Parse one session into canonical form. Takes the full `SourceFile`
    /// (not just its path) so providers can carry extra identity
    /// (rowid, archive index) through the scan → parse pipeline.
    ///
    /// Returns `None` only when the file can't be read or parsed at all. A
    /// readable session with zero billable lines should still return
    /// `Some` (an empty `ParsedSession`): the incremental cache validates
    /// by matching its session count to the scanned-file count, so a
    /// `None` for a file that keeps being scanned would force a full
    /// rebuild on every run.
    fn parse_session(&self, src: &SourceFile) -> Option<ParsedSession>;

    /// Parse one session's *message content* for the session browser.
    ///
    /// [`Source::parse_session`] feeds the aggregation cache and only
    /// needs token rows; this is the heavier second read that the TUI
    /// and web transcript views consume. Same allow-empty contract as
    /// `parse_session`: a readable log with nothing renderable returns
    /// `Some` with empty `messages` (the per-session cache stores that
    /// so the next run skips it cheaply), and `None` means the file
    /// couldn't be read at all.
    ///
    /// A provider that has no browsable transcript can return an empty
    /// `Session` unconditionally — it then contributes no sessions to
    /// the browser and drops out of the web bundle's source list, while
    /// still reporting usage through `parse_session`.
    fn parse_messages(&self, path: &Path) -> Option<crate::parse::Session>;

    /// Bucket key grouping one session into a browser "project".
    ///
    /// The default is the file's parent directory, which is right for
    /// providers that store one directory per project. Providers with a
    /// flat log layout override — Codex writes everything under
    /// `YYYY/MM/DD/`, so keying on the directory would invent one
    /// project per calendar day.
    fn project_key(&self, path: &Path, _cwd: Option<&str>) -> String {
        path.parent().unwrap_or(path).to_string_lossy().into_owned()
    }

    /// Pricing for a given model, honoring the user's config overrides.
    ///
    /// Not implemented by providers — they write [`Source::base_price`].
    /// Applying overrides here rather than in each provider is what
    /// guarantees a new provider picks them up for free instead of
    /// quietly ignoring the config.
    fn price(&self, model: Option<&str>) -> &Pricing {
        if let Some(name) = model {
            if let Some(p) = price_override(name) {
                return p;
            }
        }
        self.base_price(model)
    }

    /// This provider's own rate for a model. `None` means "unknown
    /// model" — the implementation decides the fallback.
    fn base_price(&self, model: Option<&str>) -> &Pricing;

    /// Normalize a model name for display (strip vendor prefix, date
    /// suffix, etc.). `"claude-opus-4-6-20251205"` → `"opus-4-6"`. Returns
    /// `Cow::Borrowed` when no transformation is needed, so providers
    /// whose names are already canonical pay no allocation.
    fn normalize_model<'a>(&self, model: &'a str) -> Cow<'a, str>;

    /// Should this model be skipped entirely when aggregating? Default
    /// keeps everything; providers that emit pseudo-models (e.g.
    /// Claude's `<synthetic>` compaction) override to filter.
    fn skip_model(&self, _model: &str) -> bool {
        false
    }

    /// Price tokens against this provider's rate table, returning the
    /// per-column dollar cost (`input`, `output`, `cache_write`, `cache_read`).
    /// Single arithmetic source of truth — every cost-producing site
    /// (cache build, per-session totals, JSON output) routes through
    /// this method so floating-point ordering stays identical.
    fn price_columns(&self, model: Option<&str>, t: Tokens) -> [f64; 4] {
        self.price(model).columns(t)
    }

    /// Sum-of-columns convenience for callers that don't need the split.
    fn compute_cost(&self, model: Option<&str>, t: Tokens) -> f64 {
        self.price_columns(model, t).iter().sum()
    }
}

// ── Per-model rate memoization ──

/// Per-model rate cache, indexed by the cache's `model_id`.
///
/// Resolves each interned model's pricing + skip flag exactly once. The
/// `LiteLLM` lookup (candidate-list allocation + boundary substring scan
/// over ~20k keys) is the expensive part of pricing and is identical for
/// every line of a given model, so the per-line aggregation loops resolve
/// through this table — one lookup per distinct model, not one per line.
pub struct ModelRates {
    pricing: Vec<Pricing>,
    skip: Vec<bool>,
    unknown: Pricing,
}

impl ModelRates {
    pub fn build<S: Source + ?Sized>(source: &S, models: &[String]) -> Self {
        Self {
            pricing: models.iter().map(|m| *source.price(Some(m))).collect(),
            skip: models.iter().map(|m| source.skip_model(m)).collect(),
            unknown: *source.price(None),
        }
    }

    /// `mid == u16::MAX` means "no model" — never skipped (matches the
    /// `model_name.is_some_and(skip_model)` shape it replaces).
    pub fn skip(&self, mid: u16) -> bool {
        mid != u16::MAX && self.skip.get(mid as usize).copied().unwrap_or(false)
    }

    /// Per-column cost for `mid`'s rate. `u16::MAX` (or an out-of-range
    /// id) falls back to the provider's unknown-model pricing — identical
    /// to `price_columns(None, …)`.
    pub fn columns(&self, mid: u16, t: Tokens) -> [f64; 4] {
        let p = if mid == u16::MAX {
            &self.unknown
        } else {
            self.pricing.get(mid as usize).unwrap_or(&self.unknown)
        };
        p.columns(t)
    }
}

// ── Default implementations composed from id() + logs_dir() ──

/// Per-provider cache location. `~/.claude/ccaudit-cache/{id}.db`. The
/// shared parent directory is deliberate — reusing the dir keeps us out
/// of $HOME's top level while letting multiple providers coexist.
pub fn default_cache_path(id: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".claude")
            .join("ccaudit-cache")
            .join(format!("{id}.db"))
    })
}

/// Max directory depth below `logs_dir` for a session file.
///
/// Claude Code nests subagent transcripts at `<project>/<uuid>/subagents/
/// agent-*.jsonl` (depth 4) and workflow agents at depth 6. 8 clears both
/// with headroom, and bounds the walk against symlink cycles.
pub const MAX_SCAN_DEPTH: usize = 8;

/// Portable `logs_dir` walk.
///
/// Recurses to [`MAX_SCAN_DEPTH`], collecting every `*.jsonl` with its
/// (mtime, size) fingerprint. Platform-specific overrides must match this
/// traversal or the two disagree on which sessions exist.
pub fn default_scan(dir: &Path) -> Vec<SourceFile> {
    let mut out: Vec<SourceFile> = Vec::with_capacity(256);
    scan_dir_recursive(dir, MAX_SCAN_DEPTH, &mut out);
    out
}

fn scan_dir_recursive(dir: &Path, depth_left: usize, out: &mut Vec<SourceFile>) {
    if depth_left == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        // metadata() follows symlinks, so symlinked project dirs stay
        // traversable.
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            scan_dir_recursive(&p, depth_left - 1, out);
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
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

// ── Log-root overrides ──
//
// Set once at startup from `--logs-dir` / the config file, then read by
// every `log_roots()` call. A process global rather than a field on the
// provider because the `Source` impls are zero-sized singletons handed
// out as `&'static dyn Source`; threading the paths through every call
// site instead would touch the cache, parse, report and web layers to
// carry something only the scanner needs.

static LOG_ROOTS: std::sync::OnceLock<Vec<(String, Vec<PathBuf>)>> = std::sync::OnceLock::new();

/// Install per-source log-root overrides. Only the first call takes
/// effect, which is the one `main` makes before any scan.
pub fn set_log_roots(roots: Vec<(String, Vec<PathBuf>)>) {
    let _ = LOG_ROOTS.set(roots);
}

/// Configured roots for `id`, or `None` to use the provider's default.
/// Linear scan over a list that holds one entry per provider.
fn log_root_override(id: &str) -> Option<&'static [PathBuf]> {
    LOG_ROOTS
        .get()?
        .iter()
        .find(|(k, _)| k == id)
        .map(|(_, v)| v.as_slice())
}

// ── Price overrides ──
//
// Config-supplied rates, keyed on the raw model name from the logs. Read
// on every `price()` call, so the list stays small — one entry per model
// the user chose to correct, not a copy of the LiteLLM table.

static PRICE_OVERRIDES: std::sync::OnceLock<Vec<(String, Pricing)>> = std::sync::OnceLock::new();

/// Install per-model price overrides. Only the first call takes effect,
/// which is the one `main` makes before any pricing happens.
pub fn set_price_overrides(prices: Vec<(String, Pricing)>) {
    let _ = PRICE_OVERRIDES.set(prices);
}

fn price_override(model: &str) -> Option<&'static Pricing> {
    PRICE_OVERRIDES
        .get()?
        .iter()
        .find(|(k, _)| k == model)
        .map(|(_, p)| p)
}

/// Digest of the active price overrides, or 0 when there are none.
///
/// Costs are baked into the aggregation cache at build time, so the
/// cache filename has to change when the rates do.
pub fn price_override_digest() -> u64 {
    let Some(list) = PRICE_OVERRIDES.get() else {
        return 0;
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (model, p) in list {
        for b in model.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        for v in [
            p.input,
            p.output,
            p.cache_write,
            p.cache_write_1h,
            p.cache_read,
        ] {
            for b in v.to_bits().to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    h
}

// ── Source registry ──
//
// Every provider we support appears once in `SourceKind` + `pick()`.
// CLI resolves `--source NAME` to a SourceKind, then `pick` hands back
// the singleton trait object. Adding a new provider is three lines
// here plus the provider file.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourceKind {
    #[default]
    ClaudeCode,
    Codex,
}

impl SourceKind {
    /// Every provider ccaudit knows about, in display order. Single
    /// source of truth for the web bundle's source list — a provider
    /// added to `pick` but missed here would silently never appear in
    /// the browser's dropdown.
    pub const ALL: [Self; 2] = [Self::ClaudeCode, Self::Codex];
}

impl std::str::FromStr for SourceKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude-code" | "claude" | "cc" => Ok(Self::ClaudeCode),
            "codex" | "openai" | "cdx" => Ok(Self::Codex),
            other => Err(format!(
                "unknown source {other:?}; known: claude-code (aliases: claude, cc), codex (aliases: openai, cdx)"
            )),
        }
    }
}

/// Resolve a `SourceKind` to its singleton `Source` impl.
pub fn pick(kind: SourceKind) -> &'static dyn Source {
    match kind {
        SourceKind::ClaudeCode => &claude_code::ClaudeCode,
        SourceKind::Codex => &codex::Codex,
    }
}

// ── Shared utilities (provider-agnostic) ──

/// FNV-1a 64-bit hash. Used as the canonical msg-id and path key
/// across the cache layer — collision risk negligible at our volumes,
/// zero allocation per call.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// FNV-1a hash of a path's UTF-8 representation. Stable across runs
/// for the same path string; used to identify cached sessions.
pub fn path_hash(p: &Path) -> u64 {
    fnv1a(p.to_string_lossy().as_bytes())
}

/// Days since 1970-01-01 UTC for the given timestamp.
pub fn day_from_ts(ts: DateTime<Utc>) -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    ts.date_naive().signed_duration_since(epoch).num_days() as i32
}

/// Inverse of `day_from_ts` — `NaiveDate` for a day-since-epoch index.
pub fn day_to_date(days: i32) -> NaiveDate {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    epoch
        .checked_add_signed(chrono::Duration::days(i64::from(days)))
        .unwrap_or(epoch)
}

/// Drop the leading `Users/<name>/` (macOS) or `home/<name>/` (Linux)
/// from a tokenized path.
///
/// Both Claude Code (dash-separated dir name like `-Users-me-code-foo`)
/// and Codex (slash-separated `cwd` like `/Users/me/code/foo`) share
/// this display rule once each provider tokenizes its native shape.
/// Returns `None` if the path doesn't match the `<home-root>/<name>/<rest>`
/// form so callers can fall back to the raw string.
pub fn prettify_user_path(parts: &[&str]) -> Option<String> {
    let head = parts.first().copied()?;
    if parts.len() > 2 && (head == "Users" || head == "home") {
        return parts.get(2..).map(|s| s.join("/"));
    }
    None
}

/// Strip a leading `/`, split on `/`, and run [`prettify_user_path`].
/// Falls back to the raw `cwd` string if it isn't shaped like a home dir.
pub fn prettify_cwd(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.trim_start_matches('/').split('/').collect();
    prettify_user_path(&parts).unwrap_or_else(|| cwd.to_string())
}

/// Replace control characters with spaces.
///
/// Shared by the providers so a session display name stored in the cache
/// is clean regardless of which provider produced it (renderers can still
/// defensively re-escape).
pub fn sanitize_control(s: &str) -> String {
    if !s.contains(|c: char| c.is_control()) {
        return s.to_string();
    }
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::{Pricing, Tokens};

    // Claude Opus 4.x, mirroring `claude_code::OPUS`.
    const OPUS: Pricing = Pricing {
        input: 5.0,
        output: 25.0,
        cache_write: 6.25,
        cache_write_1h: 10.0,
        cache_read: 0.50,
    };

    /// Cache writes bill at two rates, and the 1-hour count is a subset
    /// of the total rather than an amount on top of it.
    ///
    /// Reading only `cache_creation_input_tokens` and pricing all of it
    /// at the 5-minute rate under-reported real spend by 5.6% on a
    /// year of local logs, so this pins the split, not just the sum.
    #[test]
    fn cache_writes_price_per_ttl() {
        let short_ttl = OPUS.columns(Tokens {
            cache_write: 1_000_000,
            ..Tokens::default()
        });
        assert_eq!(short_ttl[2], 6.25);

        let long_ttl = OPUS.columns(Tokens {
            cache_write: 1_000_000,
            cache_write_1h: 1_000_000,
            ..Tokens::default()
        });
        assert_eq!(long_ttl[2], 10.0, "a full 1h write bills at 2x input");

        let mixed = OPUS.columns(Tokens {
            cache_write: 1_000_000,
            cache_write_1h: 400_000,
            ..Tokens::default()
        });
        assert_eq!(
            mixed[2], 7.75,
            "600k at 6.25/M plus 400k at 10.0/M — the 1h count is part of the total, not extra"
        );
    }

    /// A provider reporting more 1h tokens than total cache writes is
    /// contradicting itself; clamp rather than bill the excess or
    /// underflow the 5-minute remainder.
    #[test]
    fn nonsensical_1h_share_is_clamped() {
        let over = OPUS.columns(Tokens {
            cache_write: 1_000_000,
            cache_write_1h: 5_000_000,
            ..Tokens::default()
        });
        assert_eq!(over[2], 10.0);
    }

    /// Providers with one cache tier set both rates equal, so the split
    /// is a no-op for them however the tokens are attributed.
    #[test]
    fn single_tier_providers_are_unaffected() {
        let gpt5 = Pricing {
            input: 1.25,
            output: 10.0,
            cache_write: 1.25,
            cache_write_1h: 1.25,
            cache_read: 0.125,
        };
        let split = Tokens {
            cache_write: 800_000,
            cache_write_1h: 800_000,
            ..Tokens::default()
        };
        let flat = Tokens {
            cache_write: 800_000,
            ..Tokens::default()
        };
        assert_eq!(gpt5.columns(flat)[2], gpt5.columns(split)[2]);
    }
}
