// Box-drawing table renderer for daily/monthly/session/blocks.

use super::fmt::{
    COMPACT, LIMIT_CELL_WIDTH, NORMAL, Widths, apply_tail, format_cost, format_limit_cell,
    format_number, label_for, limit_color, normalize_model, sort_keys, title_for, write_cost,
    write_number, write_title,
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
        Bucket::Day | Bucket::Week => base.label.max(10),
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
    // Limit header embeds the dollar cap so the reader doesn't have to
    // remember the --cost-limit arg they passed.
    let limit_hdr = opts
        .cost_limit
        .map(|c| format!("Limit ({})", format_cost(c)))
        .unwrap_or_default();
    // Limit column: only on `blocks` + --cost-limit. Width fits both the
    // data cell ("XXX.X% ████████" = LIMIT_CELL_WIDTH) and the header, so
    // a large cap like `--cost-limit 1000` doesn't push the header past
    // the box border.
    let limit_w = if matches!(bucket, Bucket::Block) && opts.cost_limit.is_some() {
        LIMIT_CELL_WIDTH.max(limit_hdr.chars().count())
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

    let mut keys = apply_tail(sort_keys(rollup, bucket), opts.tail, bucket);
    super::fmt::reorder(&mut keys, rollup, bucket, opts.order);

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
        Bucket::Week => "Week",
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
    // Per-column dollar totals, summed over the exact rows rendered below
    // so the "Total Prices" row always reconciles with "Total" — including
    // under --tail, blocks --active/--recent, and non-UTC timezones.
    let mut tot_cost_in = 0.0f64;
    let mut tot_cost_out = 0.0f64;
    let mut tot_cost_cache_w = 0.0f64;
    let mut tot_cost_cache_r = 0.0f64;

    // --breakdown emits the same BucketKey once per model; group them
    // under the outer label so the date/session prints once.
    let mut current_label: Option<String> = None;
    let mut first_group = true;

    // Six scratch slots reused across rows: [in, out, cache_create,
    // cache_read, total, cost]. Each cell formatter pushes into its
    // slot, then write_row reads &str. Old code allocated six Strings
    // per row; with the scratch, allocations only happen once each
    // slot grows past its first row's width.
    let mut scratch: [String; 6] = Default::default();

    for k in &keys {
        let Some(u) = rollup.get(k) else { continue };
        tot_in += u.input;
        tot_out += u.output;
        tot_cache_w += u.cache_create;
        tot_cache_r += u.cache_read;
        tot_cost += u.cost;
        tot_cost_in += u.cost_input;
        tot_cost_out += u.cost_output;
        tot_cost_cache_w += u.cost_cache_create;
        tot_cost_cache_r += u.cost_cache_read;

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
        for s in &mut scratch {
            s.clear();
        }
        write_number(&mut scratch[0], u.input);
        write_number(&mut scratch[1], u.output);
        write_number(&mut scratch[2], u.cache_create);
        write_number(&mut scratch[3], u.cache_read);
        write_number(
            &mut scratch[4],
            u.input + u.output + u.cache_create + u.cache_read,
        );
        write_cost(&mut scratch[5], u.cost);
        write_row(
            &mut buf,
            w,
            &Row {
                label: &label_for_row,
                extra: &first_second,
                nums: [
                    &scratch[0],
                    &scratch[1],
                    &scratch[2],
                    &scratch[3],
                    &scratch[4],
                    &scratch[5],
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
    // create for heavy contexts, output for agentic loops). These are
    // summed over the same rows as "Total" above, so the four columns add
    // up to the Total Cost row exactly.
    let total_prices = tot_cost_in + tot_cost_out + tot_cost_cache_w + tot_cost_cache_r;
    buf.push_str(&mid_hline);
    write_rates_row(
        &mut buf,
        w,
        "Total Prices",
        [
            &format_cost(tot_cost_in),
            &format_cost(tot_cost_out),
            &format_cost(tot_cost_cache_w),
            &format_cost(tot_cost_cache_r),
            &format_cost(total_prices),
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

/// Plain, machine-readable rendering for `--plain`: one tab-separated
/// record per row, raw integers (no thousands separators), raw cost
/// (no `$`), no box-drawing, no color — so `grep`/`awk`/`cut` work
/// without fighting the pretty table. A single leading `#` header line
/// names the columns; everything after is data. This is clig.dev's
/// "provide a --plain for scripts" guideline.
pub fn print_plain<S: Source + ?Sized>(
    cache: &LoadedCache,
    rollup: &FxHashMap<BreakdownKey, BucketUsage>,
    opts: &Options,
    bucket: Bucket,
    source: &S,
) {
    use std::fmt::Write as _;
    let mut keys = apply_tail(sort_keys(rollup, bucket), opts.tail, bucket);
    super::fmt::reorder(&mut keys, rollup, bucket, opts.order);

    let first_col = match bucket {
        Bucket::Day => "date",
        Bucket::Week => "week",
        Bucket::Month => "month",
        Bucket::Session => "session",
        Bucket::Block => "block_start",
    };
    let second_col = if opts.instances { "projects" } else { "models" };

    let mut buf = String::with_capacity(8_192);
    let _ = writeln!(
        buf,
        "#{first_col}\t{second_col}\tinput\toutput\tcache_create\tcache_read\ttotal\tcost_usd"
    );

    for k in &keys {
        let Some(u) = rollup.get(k) else { continue };
        let label = label_for(bucket, k.0, cache, opts);
        // Second column: a comma-joined set (no leading "- ", no
        // wrapping) so the whole record stays on one line.
        let second: String = if opts.instances {
            u.projects
                .iter()
                .filter_map(|pid| cache.projects.get(pid as usize).map(String::as_str))
                .collect::<Vec<_>>()
                .join(",")
        } else if opts.breakdown && k.1 != u16::MAX {
            cache
                .models
                .get(k.1 as usize)
                .map(|m| normalize_model(source, m).into_owned())
                .unwrap_or_default()
        } else {
            u.models
                .iter()
                .filter_map(|mid| cache.models.get(mid as usize))
                .map(|m| normalize_model(source, m).into_owned())
                .collect::<Vec<_>>()
                .join(",")
        };
        let total = u.input + u.output + u.cache_create + u.cache_read;
        let _ = writeln!(
            buf,
            "{label}\t{second}\t{}\t{}\t{}\t{}\t{}\t{:.4}",
            u.input, u.output, u.cache_create, u.cache_read, total, u.cost,
        );
    }

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(buf.as_bytes());
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

// Truncate a variable-width cell (label / model / date) to `w` display
// columns with a trailing ellipsis, so an over-long session name or
// localized date can't overrun the column and break box alignment.
// Returns `Borrowed` (no allocation) for the common in-width case.
fn fit_cell(s: &str, w: usize) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if w == 0 || s.chars().count() <= w {
        return Cow::Borrowed(s);
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('…');
    Cow::Owned(out)
}

fn write_row(buf: &mut String, w: &Widths, r: &Row<'_>) {
    use std::fmt::Write as _;
    let reset = super::fmt::reset();
    let (pre, post) = if r.highlight {
        (super::fmt::yellow(), reset)
    } else {
        ("", "")
    };
    let label = fit_cell(r.label, w.label);
    let extra = fit_cell(r.extra, w.models);
    let _ = write!(
        buf,
        "│ {pre}{label:<w_label$}{post} │ {pre}{extra:<w_models$}{post}",
        w_label = w.label,
        w_models = w.models,
    );
    for (v, cw) in r.nums.iter().zip(num_widths(w).iter()) {
        let _ = write!(buf, " │ {pre}{v:>w$}{post}", w = *cw);
    }
    if w.last_activity > 0 {
        let tail = fit_cell(r.tail, w.last_activity);
        let _ = write!(buf, " │ {pre}{tail:<w$}{post}", w = w.last_activity);
    }
    if w.limit > 0 {
        let (lpre, lpost) = match r.limit_color {
            Some(c) => (c, reset),
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
    let (yellow, reset) = (super::fmt::yellow(), super::fmt::reset());
    let _ = write!(
        buf,
        "│ {yellow}{label:<w_label$}{reset} │ {blank:<w_models$}",
        w_label = w.label,
        w_models = w.models,
        blank = "",
    );
    for (v, cw) in nums.iter().zip(num_widths(w).iter()) {
        let _ = write!(buf, " │ {yellow}{v:>w$}{reset}", w = *cw);
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
    use std::fmt::Write as _;
    let (dim, reset) = (super::fmt::dim(), super::fmt::reset());
    let _ = write!(
        buf,
        "│ {dim}{label:<w_label$}{reset} │ {blank:<w_models$}",
        w_label = w.label,
        w_models = w.models,
        blank = "",
    );
    for (v, cw) in nums.iter().zip(num_widths(w).iter()) {
        let _ = write!(buf, " │ {dim}{v:>w$}{reset}", w = *cw);
    }
    if w.last_activity > 0 {
        let _ = write!(buf, " │ {blank:<w$}", w = w.last_activity, blank = "");
    }
    if w.limit > 0 {
        let _ = write!(buf, " │ {blank:<w$}", w = w.limit, blank = "");
    }
    buf.push_str(" │\n");
}
