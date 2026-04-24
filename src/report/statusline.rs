// One-line status bar output: today's cost + total tokens + active
// 5-hour block cost.

use super::fmt::{DIM, RESET, YELLOW, format_cost, format_number};
use crate::cache::{BLOCK_SECS, Bucket, FilterOpts, LoadedCache, aggregate};
use crate::cli::Options;
use crate::source::{Source, day_to_date};
use chrono::NaiveDate;

// Writes the one-line status bar summary to stdout.
#[allow(clippy::print_stdout)]
pub fn print<S: Source + ?Sized>(cache: &LoadedCache, opts: &Options, source: &S) {
    let today = day_to_date(current_day(opts.tz_offset_secs));
    let filter = FilterOpts {
        since_day: Some(to_days(today)),
        until_day: Some(to_days(today)),
        project: opts.project.as_deref(),
        tz_offset_secs: opts.tz_offset_secs,
    };
    let today_roll = aggregate(cache, Bucket::Day, &filter, false, source);

    let mut input = 0u64;
    let mut output = 0u64;
    let mut cc = 0u64;
    let mut cr = 0u64;
    let mut cost = 0.0f64;
    for u in today_roll.values() {
        input += u.input;
        output += u.output;
        cc += u.cache_create;
        cr += u.cache_read;
        cost += u.cost;
    }
    let total = input + output + cc + cr;

    // Active 5h-block cost (local clock).
    let block_filter = FilterOpts {
        tz_offset_secs: opts.tz_offset_secs,
        ..Default::default()
    };
    let block_roll = aggregate(cache, Bucket::Block, &block_filter, false, source);
    let now = chrono::Utc::now().timestamp();
    let active_cost = block_roll
        .iter()
        .find_map(|(k, u)| {
            let start = k.0.as_i64();
            if now >= start && now < start + BLOCK_SECS {
                Some(u.cost)
            } else {
                None
            }
        })
        .unwrap_or(0.0);

    println!(
        "{DIM}today{RESET} {} tok {YELLOW}{}{RESET} · {DIM}5h{RESET} {YELLOW}{}{RESET}",
        format_number(total),
        format_cost(cost),
        format_cost(active_cost),
    );
}

fn current_day(tz_offset: i32) -> i32 {
    let now = chrono::Utc::now().timestamp() + i64::from(tz_offset);
    (now.div_euclid(86_400)) as i32
}

fn to_days(d: NaiveDate) -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    d.signed_duration_since(epoch).num_days() as i32
}
