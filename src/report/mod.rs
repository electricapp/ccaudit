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

pub fn render<S: Source + ?Sized>(cache: &LoadedCache, opts: &Options, source: &S) {
    // Statusline runs its own narrowly-scoped aggregations (today + active
    // block); return early so the polled status-bar path doesn't pay for a
    // full rollup it would only throw away — and which, under `--timezone
    // Local`, is the per-line slow path.
    if matches!(opts.cmd, Cmd::Statusline) {
        statusline::print(cache, opts, source);
        return;
    }

    let bucket = match opts.cmd {
        Cmd::Weekly => Bucket::Week,
        Cmd::Monthly => Bucket::Month,
        Cmd::Session => Bucket::Session,
        Cmd::Blocks => Bucket::Block,
        _ => Bucket::Day,
    };

    let filter = FilterOpts {
        since_day: opts.since,
        until_day: opts.until,
        project: opts.project.as_deref(),
        tz_offset_secs: opts.tz_offset_secs,
    };

    let mut rollup = aggregate(cache, bucket, &filter, opts.breakdown, source);

    // `blocks --active` / `--recent` are time-window filters on the
    // already-aggregated block rows (the block key is its start ts).
    if matches!(bucket, Bucket::Block) && (opts.blocks_active || opts.blocks_recent) {
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

    if opts.json {
        json::print(cache, &rollup, opts, bucket, source);
    } else if opts.plain {
        table::print_plain(cache, &rollup, opts, bucket, source);
    } else {
        table::print(cache, &rollup, opts, bucket, source);
    }
}
