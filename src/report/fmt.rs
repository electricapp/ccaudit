// Shared renderer helpers: column widths, ANSI, number/cost formatting,
// bucket labels, key sorting, title box.

use crate::cache::{BreakdownKey, Bucket, BucketKey, BucketUsage, LoadedCache};
use crate::cli::{Cmd, Options};
use crate::source::{Source, day_to_date};
use rustc_hash::FxHashMap;

// ── Column widths ──

#[derive(Clone, Copy)]
pub struct Widths {
    pub label: usize,
    pub models: usize,
    pub input: usize,
    pub output: usize,
    pub cache_create: usize,
    pub cache_read: usize,
    pub total: usize,
    pub cost: usize,
    /// Width of the optional trailing "Last Activity" column. `0` means
    /// the column is omitted (most reports). Session reports set it to
    /// 12 to fit `YYYY-MM-DD`.
    pub last_activity: usize,
    /// Width of the optional trailing "Limit" column. `0` means the
    /// column is omitted. `blocks` with `--cost-limit` sets it to fit
    /// "XXX.X% ████████" (~15 visible chars).
    pub limit: usize,
}

pub const NORMAL: Widths = Widths {
    label: 12,
    models: 43,
    input: 9,
    output: 11,
    cache_create: 13,
    cache_read: 15,
    total: 15,
    cost: 11,
    last_activity: 0,
    limit: 0,
};

// Compact widths fit every header string (what ccusage does). Dropping
// columns would be an additional lever but makes row semantics
// confusing — keep shape, shrink cells.
pub const COMPACT: Widths = Widths {
    label: 10,
    models: 20,
    input: 7,
    output: 9,
    cache_create: 12,
    cache_read: 11,
    total: 13,
    cost: 10,
    last_activity: 0,
    limit: 0,
};

// ── ANSI ──

pub const YELLOW: &str = "\x1b[33m";
pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";
pub const DIM: &str = "\x1b[2m";
pub const RESET: &str = "\x1b[0m";

// ── Cost-limit progress bar ──

// Visible width of the "Limit" cell: "100.0% ████████" = 6 + 1 + 8 = 15.
// Kept stable regardless of percent so the column aligns.
pub const LIMIT_BAR_WIDTH: usize = 8;
pub const LIMIT_CELL_WIDTH: usize = 15;

/// Color for the limit bar: green <60%, yellow 60–80%, red ≥80%.
/// Thresholds mirror ccmonitor (Safe / Caution / Danger).
pub const fn limit_color(pct: f64) -> &'static str {
    if pct >= 80.0 {
        RED
    } else if pct >= 60.0 {
        YELLOW
    } else {
        GREEN
    }
}

/// Render a progress bar `"XXX.X% ████░░░░"` of `LIMIT_CELL_WIDTH`
/// visible columns. Percent is clamped at 999.9% so a runaway block
/// doesn't blow out the column.
pub fn format_limit_cell(pct: f64) -> String {
    let clamped = pct.clamp(0.0, 999.9);
    let filled = ((clamped / 100.0).min(1.0) * LIMIT_BAR_WIDTH as f64).round() as usize;
    let mut s = String::with_capacity(LIMIT_CELL_WIDTH * 3);
    use std::fmt::Write as _;
    let _ = write!(s, "{clamped:>5.1}% ");
    for i in 0..LIMIT_BAR_WIDTH {
        s.push(if i < filled { '█' } else { '░' });
    }
    s
}

// ── Number / cost formatting ──
//
// Underscore separator chosen because it reads cleanly in the dense
// fixed-width table cells AND matches what the TUI/web surfaces have
// always used. One canonical impl here, called by table/json/statusline
// (Rust) and mirrored by web.rs's JS regex (`replace(.../g,'_')`).

pub const THOUSANDS_SEP: char = '_';

pub fn format_number(n: u64) -> String {
    // itoa is a few ns per conversion vs. ~50ns for the default Display
    // (uses format_args! machinery). Then we splice thousands separators.
    let mut itoa_buf = itoa::Buffer::new();
    let s = itoa_buf.format(n);
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len() + b.len() / 3);
    for (i, &c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i).is_multiple_of(3) {
            out.push(THOUSANDS_SEP);
        }
        out.push(c as char);
    }
    out
}

pub fn format_cost(c: f64) -> String {
    // Fixed 2-decimal currency with thousands separators on the dollar
    // portion ($5706.77 → $5_706.77). Convert to cents to dodge fp
    // formatting drift. Negative-defensive though we don't expect them.
    let cents = (c * 100.0).round() as i64;
    let sign = if cents < 0 { "-$" } else { "$" };
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let frac = abs % 100;
    let dollars_str = format_number(dollars);
    if frac < 10 {
        format!("{sign}{dollars_str}.0{frac}")
    } else {
        format!("{sign}{dollars_str}.{frac}")
    }
}

// ── Date / datetime formatting (centralized for future locale work) ──
//
// Phase B will plumb `--locale` into the TUI/web. By routing every
// date render through these helpers, that future work is one-edit
// instead of grepping the codebase.

pub const DATE_FMT: &str = "%Y-%m-%d";
pub const DATETIME_FMT: &str = "%Y-%m-%d %H:%M";
pub const DATETIME_SHORT_FMT: &str = "%m/%d %H:%M";

pub fn format_date(d: chrono::NaiveDate, opts: &Options) -> String {
    #[cfg(feature = "locale")]
    {
        if opts.locale.is_some() {
            return d.format_localized("%x", chrono_locale(opts)).to_string();
        }
    }
    let _ = opts;
    d.format(DATE_FMT).to_string()
}

pub fn format_datetime(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format(DATETIME_FMT).to_string()
}

pub fn format_datetime_short(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format(DATETIME_SHORT_FMT).to_string()
}

// ── Labels ──

pub fn title_for<S: Source + ?Sized>(cmd: Cmd, opts: &Options, source: &S) -> String {
    let scope = match cmd {
        Cmd::Monthly => "Monthly",
        Cmd::Session => "Session",
        Cmd::Blocks => "5-Hour Blocks",
        _ => "Daily",
    };
    let base = format!("{} Token Usage Report - {scope}", source.display_name());
    if opts.tz_label != "UTC" {
        format!("{base} ({})", opts.tz_label)
    } else {
        base
    }
}

pub fn label_for(bucket: Bucket, key: BucketKey, cache: &LoadedCache, opts: &Options) -> String {
    match bucket {
        Bucket::Day => format_date(day_to_date(key.as_i64() as i32), opts),
        Bucket::Month => format_month(key),
        Bucket::Session => {
            // Session bucketing keys by project_id (matches ccusage).
            // Label uses just the last two path components joined with
            // a dash (matches ccusage stem style: "code-ccaudit",
            // "power-monitor", "subscription-server").
            let pid = key.as_i64() as usize;
            cache
                .projects
                .get(pid)
                .map(|p| ccusage_stem(p))
                .unwrap_or_default()
        }
        Bucket::Block => format_block(key, opts.tz_offset_secs),
    }
}

pub fn format_month(key: BucketKey) -> String {
    // key = year*12 + month_index
    let v = key.as_i64();
    let year = v.div_euclid(12) as i32;
    let month = v.rem_euclid(12) as u32 + 1;
    format!("{year:04}-{month:02}")
}

pub fn format_block(key: BucketKey, tz_offset: i32) -> String {
    let ts = key.as_i64() + i64::from(tz_offset);
    let dt =
        chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).unwrap_or_else(chrono::Utc::now);
    format_datetime(dt)
}

/// ccusage-style session label: last two `/`-separated components of
/// the prettified project name, joined with `-`. e.g.
/// `phonon/crates/power/monitor` → `power-monitor`,
/// `code/ccaudit` → `code-ccaudit`.
pub fn ccusage_stem(project: &str) -> String {
    let parts: Vec<&str> = project.split('/').filter(|s| !s.is_empty()).collect();
    let mut rev = parts.iter().rev().copied();
    let last = rev.next().unwrap_or("");
    match rev.next() {
        Some(prev) => format!("{prev}-{last}"),
        None => last.to_string(),
    }
}

#[cfg(feature = "locale")]
pub fn chrono_locale(opts: &Options) -> chrono::Locale {
    opts.locale
        .as_deref()
        .and_then(|l| {
            let norm = l.replace('-', "_");
            norm.as_str().try_into().ok()
        })
        .unwrap_or(chrono::Locale::POSIX)
}

// ── Sorting ──

pub fn sort_keys(
    rollup: &FxHashMap<BreakdownKey, BucketUsage>,
    bucket: Bucket,
) -> Vec<BreakdownKey> {
    let mut ks: Vec<BreakdownKey> = rollup.keys().copied().collect();
    // Sessions: sort by last-activity desc (most recent first — matches
    // ccusage). Time buckets: by time ascending.
    match bucket {
        Bucket::Session => {
            ks.sort_by(|a, b| {
                let ta = rollup.get(a).map_or(0, |u| u.last_ts);
                let tb = rollup.get(b).map_or(0, |u| u.last_ts);
                tb.cmp(&ta)
            });
        }
        _ => ks.sort(),
    }
    ks
}

/// Keep only the `tail` most recent bucket groups. Operates on
/// already-sorted keys (see `sort_keys`): time buckets are ascending
/// (keep the last N), sessions are descending (keep the first N). With
/// `--breakdown`, several keys may share a `BucketKey` — we keep all rows
/// belonging to a retained bucket so per-model sub-rows aren't split.
pub fn apply_tail(keys: Vec<BreakdownKey>, tail: Option<u32>, bucket: Bucket) -> Vec<BreakdownKey> {
    let Some(n) = tail else { return keys };
    let n = n as usize;
    if n == 0 {
        return Vec::new();
    }
    let keep_last = !matches!(bucket, Bucket::Session);
    // Single-pass: count distinct BucketKeys as we see them and, once
    // we know the trim boundary, emit only the matching BreakdownKeys.
    // keys comes in already-sorted (time: ascending, session: desc).
    // For `keep_last` we want the last n distinct; for session we want
    // the first n. Count distinct to see if we even need to trim.
    let mut seen = rustc_hash::FxHashSet::with_capacity_and_hasher(
        keys.len().min(1024),
        rustc_hash::FxBuildHasher,
    );
    let mut n_distinct = 0usize;
    for k in &keys {
        if seen.insert(k.0) {
            n_distinct += 1;
        }
    }
    if n_distinct <= n {
        return keys;
    }
    // Trim boundary: session mode keeps first n; others keep last n.
    let skip = if keep_last { n_distinct - n } else { 0 };
    let take = n;
    let mut out = Vec::with_capacity(keys.len());
    let mut seen2 =
        rustc_hash::FxHashSet::with_capacity_and_hasher(n_distinct, rustc_hash::FxBuildHasher);
    let mut idx = 0usize;
    for k in keys {
        if seen2.insert(k.0) {
            idx += 1;
        }
        // idx is 1-based position of k's bucket in distinct order.
        if idx > skip && idx <= skip + take {
            out.push(k);
        }
    }
    out
}

// ── Model display (delegates to the provider) ──

pub fn normalize_model<'a, S: Source + ?Sized>(
    source: &S,
    m: &'a str,
) -> std::borrow::Cow<'a, str> {
    source.normalize_model(m)
}

// ── Title box ──

pub fn write_title(buf: &mut String, title: &str) {
    use std::fmt::Write as _;
    let inner = title.chars().count() + 4;
    let top: String = "─".repeat(inner);
    let spaces = " ".repeat(inner);
    let _ = writeln!(buf);
    let _ = writeln!(buf, " ╭{top}╮");
    let _ = writeln!(buf, " │{spaces}│");
    let _ = writeln!(buf, " │  {title}  │");
    let _ = writeln!(buf, " │{spaces}│");
    let _ = writeln!(buf, " ╰{top}╯");
    let _ = writeln!(buf);
}
