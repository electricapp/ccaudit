// One-line status bar output: today's cost + total tokens + active
// 5-hour block cost.

use super::fmt::{format_cost, format_number};
use crate::cache::{BLOCK_SECS, Bucket, FilterOpts, LoadedCache, aggregate};
use crate::cli::Options;
use crate::source::Source;

// Writes the one-line status bar summary to stdout.
#[allow(clippy::print_stdout)]
pub fn print<S: Source + ?Sized>(cache: &LoadedCache, opts: &Options, source: &S) {
    let today = current_day(opts.tz_offset_secs);
    let project_filter_id = opts.project.as_deref().and_then(|name| {
        cache
            .projects
            .iter()
            .position(|p| p == name)
            .map(|i| i as u16)
    });

    let mut input = 0u64;
    let mut output = 0u64;
    let mut cc = 0u64;
    let mut cr = 0u64;
    let mut cost = 0.0f64;

    if opts.tz_offset_secs == 0 {
        for p in cache.preaggs() {
            if p.day != today {
                continue;
            }
            if project_filter_id.is_some() && Some(p.project_id) != project_filter_id {
                continue;
            }
            input += u64::from(p.input);
            output += u64::from(p.output);
            cc += u64::from(p.cache_create);
            cr += u64::from(p.cache_read);
            cost += p.total_cost();
        }
    } else {
        let filter = FilterOpts {
            since_day: Some(today),
            until_day: Some(today),
            project: opts.project.as_deref(),
            tz_offset_secs: opts.tz_offset_secs,
        };
        let today_roll = aggregate(cache, Bucket::Day, &filter, false, source);
        for u in today_roll.values() {
            input += u.input;
            output += u.output;
            cc += u.cache_create;
            cr += u.cache_read;
            cost += u.cost;
        }
    }
    let total = input + output + cc + cr;

    let block_filter = FilterOpts {
        project: opts.project.as_deref(),
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

    let (dim, reset, yellow) = (super::fmt::dim(), super::fmt::reset(), super::fmt::yellow());
    println!(
        "{dim}today{reset} {} tok {yellow}{}{reset} · {dim}5h{reset} {yellow}{}{reset}",
        format_number(total),
        format_cost(cost),
        format_cost(active_cost),
    );
}

fn current_day(tz_offset: i32) -> i32 {
    let now = chrono::Utc::now().timestamp() + i64::from(tz_offset);
    (now.div_euclid(86_400)) as i32
}
