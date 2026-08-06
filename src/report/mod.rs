// Report renderers (provider-agnostic).
//
// Consume a LoadedCache + aggregation results and produce output in the
// requested format (table / json / statusline, normal / compact, with or
// without breakdown by model).

mod carbon;
pub mod fmt;
mod json;
mod statusline;
mod table;

use crate::cache::{BLOCK_SECS, Bucket, FilterOpts, LoadedCache, aggregate};
use crate::cli::{Cmd, Options};
use crate::source::Source;

/// Render the requested report to stdout.
///
/// The `Err` carries the stdout write failure — callers decide policy
/// (a broken pipe is a normal way for `| head` to end; anything else
/// deserves a nonzero exit instead of reporting success with no data).
pub fn render<S: Source + ?Sized>(
    cache: &LoadedCache,
    opts: &Options,
    source: &S,
) -> std::io::Result<()> {
    // Statusline runs its own narrowly-scoped aggregations (today + active
    // block); return early so the polled status-bar path doesn't pay for a
    // full rollup it would only throw away — and which, under `--timezone
    // Local`, is the per-line slow path.
    if matches!(opts.cmd, Cmd::Statusline) {
        return statusline::print(cache, opts, source);
    }

    let bucket = bucket_for(opts.cmd);
    let filter = filter_for(opts);
    let mut rollup = aggregate(cache, bucket, &filter, opts.breakdown, source);
    retain_block_window(&mut rollup, bucket, opts);

    emit(cache, &rollup, opts, bucket, source)
}

/// Render the union of every provider that has logs (`--all`).
///
/// Takes no `cache`/`source`: each provider is loaded and aggregated by
/// its own `Source` inside [`crate::cache::aggregate_all`], and what
/// comes back is already summed. Rendering then runs against
/// [`crate::source::Union`], whose only real jobs are the report title
/// and leaving the pre-shortened model names alone.
pub fn render_all(opts: &Options) -> std::io::Result<()> {
    let bucket = bucket_for(opts.cmd);
    let filter = filter_for(opts);
    let merged = crate::cache::aggregate_all(bucket, &filter, opts.breakdown, opts.by_agent);
    let mut rollup = merged.rollup;
    retain_block_window(&mut rollup, bucket, opts);
    emit(&merged.cache, &rollup, opts, bucket, &crate::source::Union)
}

const fn bucket_for(cmd: Cmd) -> Bucket {
    match cmd {
        Cmd::Weekly => Bucket::Week,
        Cmd::Monthly => Bucket::Month,
        Cmd::Session => Bucket::Session,
        Cmd::Blocks => Bucket::Block,
        _ => Bucket::Day,
    }
}

fn filter_for(opts: &Options) -> FilterOpts<'_> {
    FilterOpts {
        since_day: opts.since,
        until_day: opts.until,
        project: opts.project.as_deref(),
        tz_offset_secs: opts.tz_offset_secs,
    }
}

/// `blocks --active` / `--recent` are time-window filters on the
/// already-aggregated block rows (the block key is its start ts).
fn retain_block_window(
    rollup: &mut rustc_hash::FxHashMap<crate::cache::BreakdownKey, crate::cache::BucketUsage>,
    bucket: Bucket,
    opts: &Options,
) {
    if !matches!(bucket, Bucket::Block) || !(opts.blocks_active || opts.blocks_recent) {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    rollup.retain(|k, _| {
        let start = k.0.as_i64();
        if opts.blocks_active {
            now >= start && now < start + BLOCK_SECS
        } else {
            // --recent: blocks started within the last 3 days.
            start >= now - 3 * 86_400
        }
    });
}

fn emit<S: Source + ?Sized>(
    cache: &LoadedCache,
    rollup: &rustc_hash::FxHashMap<crate::cache::BreakdownKey, crate::cache::BucketUsage>,
    opts: &Options,
    bucket: Bucket,
    source: &S,
) -> std::io::Result<()> {
    let result = if opts.json {
        json::print(cache, rollup, opts, bucket, source)
    } else if opts.plain {
        table::print_plain(cache, rollup, opts, bucket, source)
    } else {
        table::print(cache, rollup, opts, bucket, source)
    };
    // After the report, so the note lands under the numbers it qualifies.
    // JSON carries the same list as a field; a stderr copy would be noise.
    if !opts.json {
        warn_about_unpriced_models(opts);
    }
    result
}

/// Name every model the run could not price from a table. stderr only —
/// a report piped into `awk` must not grow a prose line.
#[allow(clippy::print_stderr)]
fn warn_about_unpriced_models(opts: &Options) {
    use crate::source::Resolution;
    if opts.quiet || opts.no_cost {
        return;
    }
    let found = crate::source::model_resolutions();
    if found.is_empty() {
        return;
    }
    let names = |want: Resolution| -> Vec<&str> {
        found
            .iter()
            .filter(|(_, r)| *r == want)
            .map(|(n, _)| n.as_str())
            .collect()
    };
    let unknown = names(Resolution::Unknown);
    if !unknown.is_empty() {
        eprintln!(
            "note: no price for {} — billed at $0, so the totals above are low: {}",
            plural(unknown.len(), "model"),
            unknown.join(", ")
        );
    }
    let estimated = names(Resolution::Estimated);
    if !estimated.is_empty() {
        eprintln!(
            "note: {} priced from its family's rate, not a table entry: {}",
            plural(estimated.len(), "model"),
            estimated.join(", ")
        );
    }
    eprintln!("Run `ccaudit refresh-prices` to pull current rates from LiteLLM.");
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}
