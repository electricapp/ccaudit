// JSON output mirroring what ccusage emits with --json.

use super::fmt::{apply_tail, label_for, sort_keys};
use crate::cache::{BLOCK_SECS, BreakdownKey, Bucket, BucketKey, BucketUsage, LoadedCache};
use crate::cli::{Cmd, Options};
use crate::source::Source;
use rustc_hash::FxHashMap;
use serde::Serialize;

#[derive(Serialize)]
struct JsonReport<'a> {
    command: &'a str,
    timezone: &'a str,
    totals: JsonTotals,
    rows: Vec<JsonRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    carbon: Option<JsonCarbon>,
}

#[derive(Serialize)]
struct JsonCarbon {
    energy_kwh: f64,
    co2_kg: f64,
    tree_years: f64,
    tree_days: f64,
    methodology: &'static str,
}

#[derive(Serialize, Default)]
struct JsonTotals {
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
    total_tokens: u64,
    cost_usd: f64,
}

#[derive(Serialize)]
struct JsonRow {
    key: String,
    model: Option<String>,
    models: Vec<String>,
    projects: Vec<String>,
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
    total_tokens: u64,
    cost_usd: f64,
    api_calls: u32,
    active: bool,
    // Populated only for `blocks` with --cost-limit. Uses the bucket's
    // total cost so --breakdown rows under the same block share a pct.
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_pct: Option<f64>,
}

pub fn print<S: Source + ?Sized>(
    cache: &LoadedCache,
    rollup: &FxHashMap<BreakdownKey, BucketUsage>,
    opts: &Options,
    bucket: Bucket,
    _source: &S,
) {
    let keys = apply_tail(sort_keys(rollup, bucket), opts.tail, bucket);
    let now = chrono::Utc::now().timestamp();

    // Bucket-total costs for --cost-limit pct (see table.rs for the
    // same construction). Empty when the column isn't active.
    let bucket_totals: FxHashMap<BucketKey, f64> =
        if matches!(bucket, Bucket::Block) && opts.cost_limit.is_some() {
            let mut m: FxHashMap<BucketKey, f64> = FxHashMap::default();
            for k in &keys {
                if let Some(u) = rollup.get(k) {
                    *m.entry(k.0).or_insert(0.0) += u.cost;
                }
            }
            m
        } else {
            FxHashMap::default()
        };

    let mut rows: Vec<JsonRow> = Vec::with_capacity(keys.len());
    let mut totals = JsonTotals::default();
    for k in &keys {
        let Some(u) = rollup.get(k) else { continue };
        totals.input += u.input;
        totals.output += u.output;
        totals.cache_create += u.cache_create;
        totals.cache_read += u.cache_read;
        totals.cost_usd += u.cost;

        let model = if opts.breakdown && k.1 != u16::MAX {
            cache.models.get(k.1 as usize).cloned()
        } else {
            None
        };
        let models: Vec<String> = u
            .models
            .iter()
            .filter_map(|id| cache.models.get(id as usize).cloned())
            .collect();
        let projects: Vec<String> = u
            .projects
            .iter()
            .filter_map(|id| cache.projects.get(id as usize).cloned())
            .collect();
        let active = matches!(bucket, Bucket::Block)
            && (now >= k.0.as_i64() && now < k.0.as_i64() + BLOCK_SECS);
        let limit_pct = opts.cost_limit.and_then(|lim| {
            if matches!(bucket, Bucket::Block) {
                let cost = bucket_totals.get(&k.0).copied().unwrap_or(u.cost);
                Some((cost / lim) * 100.0)
            } else {
                None
            }
        });
        rows.push(JsonRow {
            key: label_for(bucket, k.0, cache, opts),
            model,
            models,
            projects,
            input: u.input,
            output: u.output,
            cache_create: u.cache_create,
            cache_read: u.cache_read,
            total_tokens: u.input + u.output + u.cache_create + u.cache_read,
            cost_usd: u.cost,
            api_calls: u.line_count,
            active,
            limit_pct,
        });
    }
    totals.total_tokens = totals.input + totals.output + totals.cache_create + totals.cache_read;

    let cmd_str = match opts.cmd {
        Cmd::Monthly => "monthly",
        Cmd::Session => "session",
        Cmd::Blocks => "blocks",
        _ => "daily",
    };
    let carbon = if opts.carbon {
        let c = super::carbon::compute(
            totals.input,
            totals.output,
            totals.cache_create,
            totals.cache_read,
        );
        Some(JsonCarbon {
            energy_kwh: c.energy_kwh,
            co2_kg: c.co2_kg,
            tree_years: c.tree_years,
            tree_days: c.tree_days,
            methodology: "arXiv:2505.09598 / IEA 2024 grid avg 0.390 kg/kWh / EEA tree 14 kg/yr",
        })
    } else {
        None
    };
    let report = JsonReport {
        command: cmd_str,
        timezone: &opts.tz_label,
        totals,
        rows,
        carbon,
    };
    if let Ok(s) = serde_json::to_string_pretty(&report) {
        println!("{s}");
    }
}
