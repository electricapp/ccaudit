// Box-drawing table renderer for daily/monthly/session/blocks.

use super::fmt::{
    COMPACT, LIMIT_CELL_WIDTH, NORMAL, RESET, Widths, YELLOW, apply_tail, format_cost,
    format_limit_cell, format_number, label_for, limit_color, normalize_model, sort_keys,
    title_for, write_title,
};
use crate::cache::{BLOCK_SECS, BreakdownKey, Bucket, BucketKey, BucketUsage, LoadedCache};
use crate::cli::Options;
use crate::source::Source;
use rustc_hash::FxHashMap;
use std::io::Write;

pub fn print<S: Source + ?Sized>(
    cache: &LoadedCache,
    rollup: &FxHashMap<BreakdownKey, BucketUsage>,
    opts: &Options,
    bucket: Bucket,
    source: &S,
) {
    let base = if opts.compact { &COMPACT } else { &NORMAL };
    // Override label width per bucket; session also gets a trailing
    // "Last Activity" column to mirror ccusage.
    let label_w = match bucket {
        Bucket::Day => base.label.max(10),
        Bucket::Month => base.label.max(7),
        // Session labels are now the ccusage-style stem (e.g.
        // "code-ccaudit"), comfortably under 22 chars.
        Bucket::Session => {
            if opts.compact {
                18
            } else {
                22
            }
        }
        Bucket::Block => 16,
    };
    let last_activity = if matches!(bucket, Bucket::Session) {
        // Wide enough to fit the header text "Last Activity" (13 chars)
        // — dates ("YYYY-MM-DD" = 10) right-pad to the same column.
        13
    } else {
        0
    };
    // Limit column: only on `blocks` + --cost-limit. Width fits
    // "XXX.X% ████████" (LIMIT_CELL_WIDTH).
    let limit_w = if matches!(bucket, Bucket::Block) && opts.cost_limit.is_some() {
        LIMIT_CELL_WIDTH
    } else {
        0
    };
    let w = Widths {
        label: label_w,
        last_activity,
        limit: limit_w,
        ..*base
    };
    let w = &w;
    let mut buf = String::with_capacity(16_384);

    write_title(&mut buf, &title_for(opts.cmd, opts, source));

    let keys = apply_tail(sort_keys(rollup, bucket), opts.tail, bucket);

    // --breakdown splits one bucket into many rows (one per model); we
    // want the limit % to reflect the *bucket's* total cost, not a
    // single model's slice. Precompute bucket-total costs once.
    let bucket_totals: FxHashMap<BucketKey, f64> = if limit_w > 0 {
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
    let active_block_ts = if matches!(bucket, Bucket::Block) {
        Some(chrono::Utc::now().timestamp())
    } else {
        None
    };

    let header_label = match bucket {
        Bucket::Day => "Date",
        Bucket::Month => "Month",
        Bucket::Session => "Session",
        Bucket::Block => "Block Start",
    };
    let second_col = if opts.instances { "Projects" } else { "Models" };

    // Precompute the three separator strings once. Each `write_hline`
    // used to allocate eight `"─".repeat(n)` Strings; rendered four
    // times that's 32 small allocs we now fold into 3 reusable buffers.
    let top_hline = build_hline(w, '┌', '┬', '┐');
    let mid_hline = build_hline(w, '├', '┼', '┤');
    let bot_hline = build_hline(w, '└', '┴', '┘');

    buf.push_str(&top_hline);
    // Limit header embeds the dollar cap so the reader doesn't have to
    // remember the --cost-limit arg they passed.
    let limit_hdr = opts
        .cost_limit
        .map(|c| format!("Limit ({})", format_cost(c)))
        .unwrap_or_default();
    write_row(
        &mut buf,
        w,
        &Row {
            label: header_label,
            extra: second_col,
            nums: [
                "Input",
                "Output",
                "Cache Create",
                "Cache Read",
                "Total Tokens",
                "Cost (USD)",
            ],
            highlight: false,
            tail: "Last Activity",
            limit: &limit_hdr,
            limit_color: None,
        },
    );
    buf.push_str(&mid_hline);

    let mut tot_in = 0u64;
    let mut tot_out = 0u64;
    let mut tot_cache_w = 0u64;
    let mut tot_cache_r = 0u64;
    let mut tot_cost = 0.0f64;

    // --breakdown emits the same BucketKey once per model; group them
    // under the outer label so the date/session prints once.
    let mut current_label: Option<String> = None;
    let mut first_group = true;

    for k in &keys {
        let Some(u) = rollup.get(k) else { continue };
        tot_in += u.input;
        tot_out += u.output;
        tot_cache_w += u.cache_create;
        tot_cache_r += u.cache_read;
        tot_cost += u.cost;

        let bucket_label = label_for(bucket, k.0, cache, opts);
        let is_new_group = current_label.as_deref() != Some(bucket_label.as_str());
        if is_new_group && !first_group {
            buf.push_str(&mid_hline);
        }
        first_group = false;

        // Second column: Models list (or a single model row on --breakdown
        // per-model, or the project set if --instances).
        let mut second_lines: Vec<String> = Vec::new();
        if opts.instances {
            for pid in u.projects.iter() {
                if let Some(name) = cache.projects.get(pid as usize) {
                    second_lines.push(format!("- {name}"));
                }
            }
        } else if opts.breakdown && k.1 != u16::MAX {
            if let Some(m) = cache.models.get(k.1 as usize) {
                second_lines.push(format!("- {}", normalize_model(source, m)));
            }
        } else {
            for mid in u.models.iter() {
                if let Some(m) = cache.models.get(mid as usize) {
                    second_lines.push(format!("- {}", normalize_model(source, m)));
                }
            }
        }

        let highlight = active_block_ts
            .map(|now| {
                let start = k.0.as_i64();
                let end = start + BLOCK_SECS;
                now >= start && now < end
            })
            .unwrap_or(false);

        let label_for_row = if is_new_group {
            bucket_label.clone()
        } else {
            String::new()
        };
        let first_second = second_lines.first().cloned().unwrap_or_default();
        // "Last Activity" cell — only filled for the first sub-row of
        // the group (subsequent model rows leave it blank).
        let last_act = if is_new_group && matches!(bucket, Bucket::Session) {
            format_last_activity(u.last_ts, opts)
        } else {
            String::new()
        };
        // Limit cell: only on the first sub-row of each bucket, and
        // only when the column is active. Uses the bucket's total cost
        // (summed across model breakdown rows), not this row's slice.
        let (limit_cell, limit_col) = if limit_w > 0 && is_new_group {
            if let Some(lim) = opts.cost_limit {
                let cost = bucket_totals.get(&k.0).copied().unwrap_or(u.cost);
                let pct = (cost / lim) * 100.0;
                (format_limit_cell(pct), Some(limit_color(pct)))
            } else {
                (String::new(), None)
            }
        } else {
            (String::new(), None)
        };
        let in_fmt = format_number(u.input);
        let out_fmt = format_number(u.output);
        let cache_create_fmt = format_number(u.cache_create);
        let cache_read_fmt = format_number(u.cache_read);
        let tot_fmt = format_number(u.input + u.output + u.cache_create + u.cache_read);
        let cost_fmt = format_cost(u.cost);
        write_row(
            &mut buf,
            w,
            &Row {
                label: &label_for_row,
                extra: &first_second,
                nums: [
                    &in_fmt,
                    &out_fmt,
                    &cache_create_fmt,
                    &cache_read_fmt,
                    &tot_fmt,
                    &cost_fmt,
                ],
                highlight,
                tail: &last_act,
                limit: &limit_cell,
                limit_color: limit_col,
            },
        );
        for extra in second_lines.iter().skip(1) {
            write_row(
                &mut buf,
                w,
                &Row {
                    label: "",
                    extra,
                    nums: ["", "", "", "", "", ""],
                    highlight,
                    tail: "",
                    limit: "",
                    limit_color: None,
                },
            );
        }

        current_label = Some(bucket_label);
    }

    // Totals
    buf.push_str(&mid_hline);
    write_total(
        &mut buf,
        w,
        "Total",
        [
            &format_number(tot_in),
            &format_number(tot_out),
            &format_number(tot_cache_w),
            &format_number(tot_cache_r),
            &format_number(tot_in + tot_out + tot_cache_w + tot_cache_r),
            &format_cost(tot_cost),
        ],
    );

    // Per-column dollar cost. We already know token counts per column;
    // this row shows the dollars attributable to each column so you can
    // see which token type is actually driving spend (usually cache-
    // create for heavy contexts, output for agentic loops). Summed
    // across columns they equal the Total Cost row above.
    let filter = crate::cache::FilterOpts {
        since_day: opts.since,
        until_day: opts.until,
        project: opts.project.as_deref(),
        tz_offset_secs: opts.tz_offset_secs,
    };
    let _ = source; // no longer used at render time (per-column costs baked into preaggs)
    let costs = column_costs(cache, &filter);
    buf.push_str(&mid_hline);
    write_rates_row(
        &mut buf,
        w,
        "Total Prices",
        [
            &format_cost(costs.input),
            &format_cost(costs.output),
            &format_cost(costs.cache_create),
            &format_cost(costs.cache_read),
            &format_cost(costs.total),
            "", // cost column intentionally left blank (it's in the row above)
        ],
    );
    buf.push_str(&bot_hline);

    if opts.carbon {
        let c = super::carbon::compute(tot_in, tot_out, tot_cache_w, tot_cache_r);
        // Visible width of the table = chars in the precomputed top hline
        // minus the trailing newline. Used by the carbon footer to align
        // its border under the table.
        let box_width = top_hline.chars().count().saturating_sub(1);
        super::carbon::write_footer(&mut buf, &c, box_width);
    }

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(buf.as_bytes());
}

// ── Per-column cost breakdown ──

struct ColumnCosts {
    input: f64,        // dollars for all input tokens
    output: f64,       // dollars for all output tokens
    cache_create: f64, // dollars for all cache-write tokens
    cache_read: f64,   // dollars for all cache-read tokens
    total: f64,        // sum of the above; equals the Total Cost row
}

fn column_costs(cache: &LoadedCache, filter: &crate::cache::FilterOpts) -> ColumnCosts {
    // Per-column costs are baked into each preagg at build time — this
    // just sums them with the same filter as the aggregator above.
    // Zero price lookups, no prices.json parse on the hot path.
    let project_id = filter.project.and_then(|name| {
        cache
            .projects
            .iter()
            .position(|p| p == name)
            .map(|i| i as u16)
    });

    let mut c = ColumnCosts {
        input: 0.0,
        output: 0.0,
        cache_create: 0.0,
        cache_read: 0.0,
        total: 0.0,
    };

    for p in cache.preaggs() {
        if let Some(s) = filter.since_day {
            if p.day < s {
                continue;
            }
        }
        if let Some(u) = filter.until_day {
            if p.day > u {
                continue;
            }
        }
        if project_id.is_some() && Some(p.project_id) != project_id {
            continue;
        }
        c.input += p.cost_input;
        c.output += p.cost_output;
        c.cache_create += p.cost_cache_create;
        c.cache_read += p.cost_cache_read;
    }
    c.total = c.input + c.output + c.cache_create + c.cache_read;
    c
}

// Format last-activity cell as a date string from a unix-seconds value,
// honoring `--locale` via the centralized `format_date` helper. Empty
// when the bucket carried no timestamp.
fn format_last_activity(ts_unix: i64, opts: &Options) -> String {
    if ts_unix <= 0 {
        return String::new();
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts_unix, 0)
        .map(|d| super::fmt::format_date(d.date_naive(), opts))
        .unwrap_or_default()
}

// ── Row primitives ──

const fn num_widths(w: &Widths) -> [usize; 6] {
    [
        w.input,
        w.output,
        w.cache_create,
        w.cache_read,
        w.total,
        w.cost,
    ]
}

// Build a horizontal separator line once. Used by `print` to precompute
// the top/middle/bottom borders, then pushed into the output buffer
// each time we need one — instead of re-allocating the eight `seg`
// strings every render.
fn build_hline(w: &Widths, left: char, mid: char, right: char) -> String {
    let cap = (w.label
        + w.models
        + w.input
        + w.output
        + w.cache_create
        + w.cache_read
        + w.total
        + w.cost
        + w.last_activity
        + w.limit
        + 20)
        * 3
        + 32;
    let mut s = String::with_capacity(cap);
    s.push(left);
    push_seg(&mut s, w.label);
    s.push(mid);
    push_seg(&mut s, w.models);
    for c in num_widths(w) {
        s.push(mid);
        push_seg(&mut s, c);
    }
    if w.last_activity > 0 {
        s.push(mid);
        push_seg(&mut s, w.last_activity);
    }
    if w.limit > 0 {
        s.push(mid);
        push_seg(&mut s, w.limit);
    }
    s.push(right);
    s.push('\n');
    s
}

fn push_seg(s: &mut String, n: usize) {
    // Append `n + 2` copies of "─" (3 bytes each) directly into the
    // pre-sized buffer. Avoids the intermediate `String` that
    // `"─".repeat` would allocate.
    for _ in 0..(n + 2) {
        s.push('─');
    }
}

/// Data for one table row. Grouped so `write_row` doesn't need a
/// 9-arg signature. All borrows are short-lived and Copy where
/// possible, so building a struct per row is zero-cost.
struct Row<'a> {
    label: &'a str,
    extra: &'a str,
    nums: [&'a str; 6],
    tail: &'a str,
    limit: &'a str,
    highlight: bool,
    /// Per-row color override for the limit cell (green/yellow/red).
    /// `None` means fall back to the row's highlight color.
    limit_color: Option<&'static str>,
}

fn write_row(buf: &mut String, w: &Widths, r: &Row<'_>) {
    use std::fmt::Write as _;
    let (pre, post) = if r.highlight {
        (YELLOW, RESET)
    } else {
        ("", "")
    };
    let _ = write!(
        buf,
        "│ {pre}{label:<w_label$}{post} │ {pre}{extra:<w_models$}{post}",
        label = r.label,
        extra = r.extra,
        w_label = w.label,
        w_models = w.models,
    );
    for (v, cw) in r.nums.iter().zip(num_widths(w).iter()) {
        let _ = write!(buf, " │ {pre}{v:>w$}{post}", w = *cw);
    }
    if w.last_activity > 0 {
        let _ = write!(
            buf,
            " │ {pre}{tail:<w$}{post}",
            tail = r.tail,
            w = w.last_activity,
        );
    }
    if w.limit > 0 {
        let (lpre, lpost) = match r.limit_color {
            Some(c) => (c, RESET),
            None => (pre, post),
        };
        let _ = write!(
            buf,
            " │ {lpre}{limit:<w$}{lpost}",
            limit = r.limit,
            w = w.limit
        );
    }
    buf.push_str(" │\n");
}

fn write_total(buf: &mut String, w: &Widths, label: &str, nums: [&str; 6]) {
    use std::fmt::Write as _;
    let _ = write!(
        buf,
        "│ {YELLOW}{label:<w_label$}{RESET} │ {blank:<w_models$}",
        w_label = w.label,
        w_models = w.models,
        blank = "",
    );
    for (v, cw) in nums.iter().zip(num_widths(w).iter()) {
        let _ = write!(buf, " │ {YELLOW}{v:>w$}{RESET}", w = *cw);
    }
    if w.last_activity > 0 {
        let _ = write!(buf, " │ {blank:<w$}", w = w.last_activity, blank = "");
    }
    if w.limit > 0 {
        let _ = write!(buf, " │ {blank:<w$}", w = w.limit, blank = "");
    }
    buf.push_str(" │\n");
}

// Dim counterpart to write_total, used for the "Total Prices" row so
// it reads as supplementary context instead of a second primary total.
fn write_rates_row(buf: &mut String, w: &Widths, label: &str, nums: [&str; 6]) {
    use super::fmt::DIM;
    use std::fmt::Write as _;
    let _ = write!(
        buf,
        "│ {DIM}{label:<w_label$}{RESET} │ {blank:<w_models$}",
        w_label = w.label,
        w_models = w.models,
        blank = "",
    );
    for (v, cw) in nums.iter().zip(num_widths(w).iter()) {
        let _ = write!(buf, " │ {DIM}{v:>w$}{RESET}", w = *cw);
    }
    if w.last_activity > 0 {
        let _ = write!(buf, " │ {blank:<w$}", w = w.last_activity, blank = "");
    }
    if w.limit > 0 {
        let _ = write!(buf, " │ {blank:<w$}", w = w.limit, blank = "");
    }
    buf.push_str(" │\n");
}
