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

use crate::cache::{Bucket, FilterOpts, LoadedCache, aggregate};
use crate::cli::{Cmd, Options};
use crate::source::Source;

pub fn render<S: Source + ?Sized>(cache: &LoadedCache, opts: &Options, source: &S) {
    let bucket = match opts.cmd {
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

    let rollup = aggregate(cache, bucket, &filter, opts.breakdown, source);

    match opts.cmd {
        Cmd::Statusline => statusline::print(cache, opts, source),
        _ => {
            if opts.json {
                json::print(cache, &rollup, opts, bucket, source);
            } else {
                table::print(cache, &rollup, opts, bucket, source);
            }
        }
    }
}
