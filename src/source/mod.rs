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
    pub cache_create: u32,
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

// Per-million-token prices. A provider returns one of these for each
// model it knows about; unknown models fall back to the provider's
// default (typically Sonnet-tier).
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
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

    /// Directory where this provider's logs live. `None` when the
    /// platform doesn't expose a home dir (rare). Providers whose
    /// sessions aren't filesystem-rooted (e.g. a SQLite-backed provider)
    /// return the containing directory of the db.
    fn logs_dir(&self) -> Option<PathBuf>;

    /// Binary cache file path. Default: `{cache_root}/{id()}.db`. Override
    /// only to put the cache somewhere non-standard.
    fn cache_path(&self) -> Option<PathBuf> {
        default_cache_path(self.id())
    }

    /// Enumerate every session available from this provider without
    /// parsing. Default walks `logs_dir()` and yields every `*.jsonl`
    /// file. Providers with non-file layouts (`SQLite`, archive, etc.)
    /// override to synthesize one `SourceFile` per session, stashing
    /// whatever identity info they need in `path_hash` + `path`.
    fn scan_sources(&self) -> Vec<SourceFile> {
        self.logs_dir()
            .as_deref()
            .map(default_scan)
            .unwrap_or_default()
    }

    /// Parse one session into canonical form. Takes the full `SourceFile`
    /// (not just its path) so providers can carry extra identity
    /// (rowid, archive index) through the scan → parse pipeline. Returns
    /// None if the session is empty or malformed.
    fn parse_session(&self, src: &SourceFile) -> Option<ParsedSession>;

    /// Pricing for a given model. `None` means "unknown model" — the
    /// implementation decides the fallback.
    fn price(&self, model: Option<&str>) -> &Pricing;

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

    /// Convenience: price tokens against this provider's rate table.
    fn compute_cost(
        &self,
        model: Option<&str>,
        input: u64,
        output: u64,
        cache_write: u64,
        cache_read: u64,
    ) -> f64 {
        // Fused multiply-add: each term adds to the running total in
        // one rounded op. Clippy's `suboptimal_flops` was flagging the
        // naïve sum; `mul_add` is the cleaner expression of the same.
        let p = self.price(model);
        let t = (input as f64).mul_add(p.input, 0.0);
        let t = (output as f64).mul_add(p.output, t);
        let t = (cache_write as f64).mul_add(p.cache_write, t);
        let t = (cache_read as f64).mul_add(p.cache_read, t);
        t / 1_000_000.0
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

/// Portable `logs_dir` walk: readdir the outer directory, then for each
/// subdir readdir + stat every `*.jsonl`. ~1 stat syscall per file. Used
/// as the default `scan_sources` implementation; providers with a faster
/// platform-specific path (see `ClaudeCode`'s macOS bulk-scan) override.
pub fn default_scan(dir: &Path) -> Vec<SourceFile> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut out: Vec<SourceFile> = Vec::with_capacity(256);
    for e in entries.flatten() {
        let d = e.path();
        let Ok(sub) = fs::read_dir(&d) else { continue };
        for f in sub.flatten() {
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
    out
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
    // Future: Codex, OpenCode, Pi, Amp
}

impl SourceKind {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "claude-code" | "claude" | "cc" => Ok(Self::ClaudeCode),
            other => Err(format!(
                "unknown source {other:?}; known: claude-code (aliases: claude, cc)"
            )),
        }
    }
}

pub fn pick(kind: SourceKind) -> &'static dyn Source {
    match kind {
        SourceKind::ClaudeCode => &claude_code::ClaudeCode,
    }
}

// ── Shared utilities (provider-agnostic) ──

pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

pub fn path_hash(p: &Path) -> u64 {
    fnv1a(p.to_string_lossy().as_bytes())
}

pub fn day_from_ts(ts: DateTime<Utc>) -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    ts.date_naive().signed_duration_since(epoch).num_days() as i32
}

pub fn day_to_date(days: i32) -> NaiveDate {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    epoch
        .checked_add_signed(chrono::Duration::days(i64::from(days)))
        .unwrap_or(epoch)
}
