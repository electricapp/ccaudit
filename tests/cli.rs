// Integration tests for the ccaudit CLI surface.
//
// Every subcommand + every flag exercised against a synthetic fixture.
// Pricing math is deterministic so cost assertions use exact numbers.
//
// Fixture layout per test: one or two projects, each with a single
// session containing assistant lines at known timestamps.
//
//   opus: $5/M in, $25/M out, $6.25/M cache-write, $0.50/M cache-read
//   Line A: 1000 in + 2000 out + 0 cw + 0 cr      → $0.0550
//   Line B: 500 in + 500 out + 10000 cw + 1000 cr → $0.0780
//   Total                                          $0.1330

// Tests use `.unwrap()`, index slicing, and integer literals freely —
// the usual clippy warnings would fight the test-writing style, so the
// relevant lints are muted at the file scope.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreadable_literal,
    unused_results,
    unused_qualifications
)]

mod common;

use common::*;
use serde_json::Value;
use std::process::Output;

fn require_success(out: &Output, ctx: &str) {
    assert!(
        out.status.success(),
        "{ctx} failed: exit={:?}\nSTDOUT:\n{}\nSTDERR:\n{}",
        out.status.code(),
        read_stdout(out),
        read_stderr(out)
    );
}

/// Usage errors must exit with code 2 exactly — `!success()` alone would
/// also accept a panic/abort (exit 101 / signal), which is how a crash in
/// input handling once slipped past "invalid input exits nonzero" tests.
/// Returns stderr so callers can pin the message.
fn require_usage_error(out: &Output, ctx: &str) -> String {
    assert_eq!(
        out.status.code(),
        Some(2),
        "{ctx}: expected clean usage error (exit 2), got exit={:?}\nSTDOUT:\n{}\nSTDERR:\n{}",
        out.status.code(),
        read_stdout(out),
        read_stderr(out)
    );
    read_stderr(out)
}

fn setup_single_project(h: &Harness) {
    let _ = h.write_jsonl(
        "-Users-test-code-alpha",
        "sess_alpha",
        &[
            &summary_line("Alpha session"),
            &user_line("hello", "2026-04-01T12:00:00.000Z"),
            &assistant_line(&AssistantLine {
                msg_id: "msg_A",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-01T12:00:01.000Z",
                input: 1000,
                output: 2000,
                cache_read: 0,
                cache_create: 0,
                text: "first reply",
            }),
            &user_line("more", "2026-04-02T09:00:00.000Z"),
            &assistant_line(&AssistantLine {
                msg_id: "msg_B",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-02T09:00:01.000Z",
                input: 500,
                output: 500,
                cache_read: 1000,
                cache_create: 10000,
                text: "second reply",
            }),
        ],
    );
}

fn setup_two_projects(h: &Harness) {
    setup_single_project(h);
    let _ = h.write_jsonl(
        "-Users-test-code-beta",
        "sess_beta",
        &[
            &summary_line("Beta session"),
            &user_line("hi beta", "2026-04-02T15:00:00.000Z"),
            &assistant_line(&AssistantLine {
                msg_id: "msg_C",
                model: "claude-sonnet-4-6",
                iso_ts: "2026-04-02T15:00:01.000Z",
                input: 100,
                output: 200,
                cache_read: 0,
                cache_create: 0,
                text: "beta reply",
            }),
        ],
    );
}

// ── Subcommand: daily (default) ──

#[test]
fn daily_default_renders_table() {
    let h = Harness::new();
    setup_single_project(&h);

    let out = h.run(&[]);
    require_success(&out, "daily default");
    let stdout = read_stdout(&out);

    assert!(stdout.contains("Claude Code Token Usage Report - Daily"));
    assert!(stdout.contains("Date"));
    assert!(stdout.contains("Input"));
    assert!(stdout.contains("Total"));
    assert!(stdout.contains("2026-04-01"));
    assert!(stdout.contains("2026-04-02"));
    assert!(stdout.contains("opus-4-7"));
}

#[test]
fn daily_with_json() {
    let h = Harness::new();
    setup_single_project(&h);

    let out = h.run(&["--json"]);
    require_success(&out, "daily --json");
    let stdout = read_stdout(&out);
    let v: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(v["command"], "daily");
    assert_eq!(v["rows"].as_array().unwrap().len(), 2);
    let totals = &v["totals"];
    // Fixture: line A (1000, 2000, cr=0, cw=0) + line B (500, 500, cr=1000, cw=10000).
    assert_eq!(totals["input"], 1500);
    assert_eq!(totals["output"], 2500);
    assert_eq!(totals["cache_read"], 1000);
    assert_eq!(totals["cache_create"], 10000);
}

#[test]
fn daily_breakdown_adds_model_rows() {
    let h = Harness::new();
    setup_two_projects(&h);

    // Without breakdown, 2026-04-02 collapses both models into one row.
    let plain = read_stdout(&h.run(&[]));
    // With breakdown, each (day, model) pair is a separate JSON row.
    let out = h.run(&["--json", "--breakdown"]);
    require_success(&out, "daily --breakdown --json");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    // 2026-04-01: opus-only (1 row). 2026-04-02: opus + sonnet (2 rows).
    assert_eq!(rows.len(), 3);
    let models: Vec<String> = rows
        .iter()
        .filter_map(|r| r["model"].as_str().map(str::to_string))
        .collect();
    assert!(models.iter().any(|m| m.contains("opus")));
    assert!(models.iter().any(|m| m.contains("sonnet")));
    assert!(plain.contains("Daily"));
}

#[test]
fn daily_compact_uses_narrower_widths() {
    let h = Harness::new();
    setup_single_project(&h);
    let normal = read_stdout(&h.run(&[]));
    let compact = read_stdout(&h.run(&["--compact"]));
    // The compact table is shorter per line than normal — compare max
    // line widths as a proxy.
    let normal_max = normal.lines().map(str::len).max().unwrap_or(0);
    let compact_max = compact.lines().map(str::len).max().unwrap_or(0);
    assert!(
        compact_max < normal_max,
        "compact should be narrower than normal ({compact_max} vs {normal_max})"
    );
}

// ── Filters ──

#[test]
fn since_until_restricts_rows() {
    let h = Harness::new();
    setup_single_project(&h);

    let out = h.run(&["--json", "--since", "20260402"]);
    require_success(&out, "--since filter");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "2026-04-02");

    let out = h.run(&["--json", "--until", "20260401"]);
    require_success(&out, "--until filter");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "2026-04-01");
}

#[test]
fn project_filter_selects_single_project() {
    let h = Harness::new();
    setup_two_projects(&h);

    let out = h.run(&["--json", "--project", "code/beta"]);
    require_success(&out, "--project filter");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    assert_eq!(v["totals"]["input"], 100);
    assert_eq!(v["totals"]["output"], 200);
}

#[test]
fn instances_groups_by_project() {
    let h = Harness::new();
    setup_two_projects(&h);

    let out = h.run(&["--json", "--instances"]);
    require_success(&out, "--instances");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    // With --instances, each row carries the projects set; across 2026-04-02
    // both alpha + beta appear under that day.
    let rows = v["rows"].as_array().unwrap();
    let apr2 = rows.iter().find(|r| r["key"] == "2026-04-02").unwrap();
    let projects = apr2["projects"].as_array().unwrap();
    assert!(projects.iter().any(|p| p.as_str() == Some("code/alpha")));
    assert!(projects.iter().any(|p| p.as_str() == Some("code/beta")));
}

// ── Subcommands: monthly / session / blocks ──

#[test]
fn monthly_groups_by_month() {
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["monthly", "--json"]);
    require_success(&out, "monthly --json");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    assert_eq!(v["command"], "monthly");
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "2026-04");
}

#[test]
fn session_groups_by_project_like_ccusage() {
    // ccusage's `session` view rolls every session for a project into
    // one row keyed by project slug — not per-jsonl-file. Verify that
    // two projects → exactly two rows, labels are project names.
    let h = Harness::new();
    setup_two_projects(&h);
    let out = h.run(&["session", "--json"]);
    require_success(&out, "session --json");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    assert_eq!(v["command"], "session");
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);

    let labels: Vec<&str> = rows.iter().map(|r| r["key"].as_str().unwrap()).collect();
    // ccusage-style stem: last two `/`-separated path components joined
    // with `-`. Project "code/alpha" → "code-alpha".
    assert!(
        labels.contains(&"code-alpha"),
        "expected ccusage-style stem as session label, got {labels:?}"
    );
    assert!(labels.contains(&"code-beta"));
}

#[test]
fn session_table_is_aligned_when_display_name_has_newline() {
    // Defensive: a user message containing a literal newline must not
    // split the cell and shred the box-drawing alignment. Verify every
    // row is the same width.
    let h = Harness::new();
    let _ = h.write_jsonl(
        "-Users-test-code-newline",
        "sess_nl",
        &[
            // Note the literal \n inside the user content.
            &common::user_line("first line\nsecond line\nthird", "2026-04-01T12:00:00.000Z"),
            &common::assistant_line(&common::AssistantLine {
                msg_id: "msg_NL",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-01T12:00:01.000Z",
                input: 100,
                output: 100,
                cache_read: 0,
                cache_create: 0,
                text: "ok",
            }),
            &common::summary_line("multiline\nsummary"),
        ],
    );
    let out = h.run(&["session"]);
    require_success(&out, "session table");
    let stdout = read_stdout(&out);
    // Strip ANSI escape sequences so we measure visible terminal columns,
    // not byte counts. Total / Total-Prices rows contain color escapes
    // that don't take a column.
    let strip_ansi = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Consume "[...m" CSI sequence
                if chars.peek() == Some(&'[') {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == 'm' {
                            break;
                        }
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    // Filter to actual data-table lines (title box has 2 │ chars, data
    // rows have 9 │, hlines have 7 ┼).
    let widths: Vec<usize> = stdout
        .lines()
        .filter(|l| l.matches('│').count() >= 8 || l.matches('┼').count() >= 7)
        .map(|l| strip_ansi(l).chars().count())
        .collect();
    // The filter must actually match table rows — otherwise this test is
    // vacuously green (it once asserted nothing when the glyph filter
    // missed, which would also hide a renderer that stopped drawing rows).
    assert!(
        !widths.is_empty(),
        "no table rows matched the box-glyph filter; full output:\n{stdout}"
    );
    let first = widths[0];
    for (i, &w) in widths.iter().enumerate() {
        assert_eq!(
            w, first,
            "row {i} width {w} != expected {first}; full output:\n{stdout}"
        );
    }
}

#[test]
fn blocks_reports_five_hour_windows() {
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["blocks", "--json"]);
    require_success(&out, "blocks --json");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    assert_eq!(v["command"], "blocks");
    // Two assistant messages on different days → two distinct 5h blocks.
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for r in rows {
        // Block label is a timestamp "YYYY-MM-DD HH:MM".
        let key = r["key"].as_str().unwrap();
        assert!(key.len() >= 16, "expected timestamp-like key, got {key}");
    }
}

#[test]
fn blocks_cost_limit_adds_pct_to_json() {
    // Fixture cost = $0.1330 → 13.3% of $1.00 limit. Each block inherits
    // its own bucket-total pct; two blocks → two pct values summing to
    // the overall total when both rows are visible.
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["blocks", "--json", "--cost-limit", "1"]);
    require_success(&out, "blocks --cost-limit");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for r in rows {
        let pct = r["limit_pct"].as_f64().expect("limit_pct present");
        assert!(pct > 0.0 && pct < 100.0, "expected in-range pct, got {pct}");
    }
    // Totals: row pcts sum to ~total_cost / limit * 100 = 13.3%.
    let sum: f64 = rows.iter().map(|r| r["limit_pct"].as_f64().unwrap()).sum();
    assert!((sum - 13.30).abs() < 1e-6, "expected ~13.30%, got {sum}");
}

#[test]
fn cost_limit_dollar_prefix_is_accepted() {
    // Users will paste "$10" from docs; strip the leading $ automatically.
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["blocks", "--json", "--cost-limit", "$1"]);
    require_success(&out, "--cost-limit $1");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    assert!(v["rows"][0]["limit_pct"].as_f64().is_some());
}

#[test]
fn cost_limit_rejected_on_non_blocks() {
    // --cost-limit is blocks-only. Silently ignoring it on other
    // commands reads as "it did something" — reject it up front.
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["--cost-limit", "1"]);
    require_usage_error(&out, "daily --cost-limit should error");
    let out = h.run(&["session", "--cost-limit", "1"]);
    require_usage_error(&out, "session --cost-limit should error");
}

#[test]
fn cost_limit_invalid_exits_nonzero() {
    let h = Harness::new();
    let out = h.run(&["blocks", "--cost-limit", "abc"]);
    require_usage_error(&out, "non-numeric limit");
    let out = h.run(&["blocks", "--cost-limit", "0"]);
    require_usage_error(&out, "zero limit should be rejected");
    let out = h.run(&["blocks", "--cost-limit", "-5"]);
    require_usage_error(&out, "negative limit should be rejected");
}

#[test]
fn blocks_cost_limit_renders_progress_bar() {
    // Sanity: table output includes a Limit column header and at least
    // one bar character when --cost-limit is set on blocks.
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["blocks", "--cost-limit", "1"]);
    require_success(&out, "blocks --cost-limit (table)");
    let stdout = read_stdout(&out);
    assert!(
        stdout.contains("Limit"),
        "expected 'Limit' column header, got:\n{stdout}"
    );
    assert!(
        stdout.contains('█') || stdout.contains('░'),
        "expected progress-bar glyph in table output"
    );
}

#[test]
fn tail_keeps_last_n_rows() {
    // Three days of data; --tail 2 should keep the two most recent.
    let h = Harness::new();
    setup_single_project(&h);
    h.write_jsonl(
        "-Users-test-code-alpha",
        "sess_alpha_day3",
        &[
            &user_line("day 3", "2026-04-03T10:00:00.000Z"),
            &assistant_line(&AssistantLine {
                msg_id: "msg_C_day3",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-03T10:00:01.000Z",
                input: 100,
                output: 100,
                cache_read: 0,
                cache_create: 0,
                text: "day 3 reply",
            }),
        ],
    );

    let out = h.run(&["--json", "--tail", "2"]);
    require_success(&out, "--tail 2");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // For time buckets (ascending sort), --tail keeps the most recent N
    // at the end of the list.
    assert_eq!(rows[0]["key"], "2026-04-02");
    assert_eq!(rows[1]["key"], "2026-04-03");
}

#[test]
fn tail_totals_reflect_visible_rows_only() {
    // Totals row should sum only the rows we're showing — otherwise the
    // visible column totals don't match the printed totals row.
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["--json", "--tail", "1"]);
    require_success(&out, "--tail 1 totals");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    // Kept row is 2026-04-02: line B only → input 500, output 500.
    assert_eq!(v["totals"]["input"], 500);
    assert_eq!(v["totals"]["output"], 500);
}

#[test]
fn tail_zero_produces_empty_rows() {
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["--json", "--tail", "0"]);
    require_success(&out, "--tail 0");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    assert_eq!(v["rows"].as_array().unwrap().len(), 0);
    assert_eq!(v["totals"]["input"], 0);
}

#[test]
fn tail_session_keeps_most_recent_by_last_activity() {
    // Sessions sort by last-activity descending; --tail must keep the
    // most-recent N, which means the *first* N in the sorted list.
    let h = Harness::new();
    setup_two_projects(&h);
    let out = h.run(&["session", "--json", "--tail", "1"]);
    require_success(&out, "session --tail 1");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    // Beta (2026-04-02T15:00) is later than alpha's last msg (09:00).
    assert_eq!(rows[0]["key"], "code-beta");
}

#[test]
fn tail_with_breakdown_keeps_all_model_subrows() {
    // With --breakdown, a bucket expands into multiple rows — --tail
    // operates on bucket groups, not row count.
    let h = Harness::new();
    setup_two_projects(&h);
    let out = h.run(&["--json", "--tail", "1", "--breakdown"]);
    require_success(&out, "--tail 1 --breakdown");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    // 2026-04-02 has both opus + sonnet → 2 breakdown rows kept.
    assert_eq!(rows.len(), 2);
    for r in rows {
        assert_eq!(r["key"], "2026-04-02");
    }
}

#[test]
fn statusline_prints_single_line() {
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["statusline"]);
    require_success(&out, "statusline");
    let stdout = read_stdout(&out);
    // One printable line (plus trailing newline). Ignore ANSI bytes.
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "statusline should be one line, got {lines:?}"
    );
    assert!(stdout.contains("today"));
    assert!(stdout.contains("5h"));
}

// ── Timezone / locale ──

#[test]
fn timezone_utc_is_default() {
    let h = Harness::new();
    setup_single_project(&h);
    let default = h.run(&["--json"]);
    let utc = h.run(&["--json", "--timezone", "UTC"]);
    require_success(&default, "default TZ");
    require_success(&utc, "--timezone UTC");
    let d: Value = serde_json::from_str(&read_stdout(&default)).unwrap();
    let u: Value = serde_json::from_str(&read_stdout(&utc)).unwrap();
    assert_eq!(d["timezone"], "UTC");
    assert_eq!(u["timezone"], "UTC");
    assert_eq!(d["totals"], u["totals"]);
}

#[test]
fn timezone_fixed_offset_accepted() {
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["--json", "--timezone", "+09:00"]);
    require_success(&out, "--timezone +09:00");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    assert_eq!(v["timezone"], "+09:00");
}

// ── Cost math (regression guard) ──

#[test]
fn cost_math_matches_pricing_table() {
    let h = Harness::new();
    setup_single_project(&h);

    let out = h.run(&["--json"]);
    require_success(&out, "cost math");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();

    // opus-4-7 priced as opus: $5 in, $25 out, $6.25 cw, $0.50 cr per million.
    //   input  1500 × 5    / 1e6 = 0.0075
    //   output 2500 × 25   / 1e6 = 0.0625
    //   cw    10000 × 6.25 / 1e6 = 0.0625
    //   cr     1000 × 0.5  / 1e6 = 0.0005
    //                            = 0.1330
    let cost = v["totals"]["cost_usd"].as_f64().unwrap();
    assert!(
        (cost - 0.1330).abs() < 1e-9,
        "expected $0.1330 total, got {cost}"
    );
}

// ── CLI meta ──

#[test]
fn help_exits_cleanly() {
    let h = Harness::new();
    let out = h.run(&["--help"]);
    require_success(&out, "--help");
    // Requested help is the command's primary output → it goes to
    // stdout (clig.dev), so `ccaudit help | less` works.
    let stdout = read_stdout(&out);
    assert!(
        !stdout.is_empty(),
        "requested --help must print to stdout, got empty stdout"
    );
    let combined = format!("{}{}", stdout, read_stderr(&out));
    assert!(combined.contains("daily"));
    assert!(combined.contains("weekly"));
    assert!(combined.contains("monthly"));
    assert!(combined.contains("statusline"));
    // The new shape advertises tui/web as real subcommands and a hint to
    // ask each one for its own help.
    assert!(combined.contains("tui"));
    assert!(combined.contains("web"));
    assert!(combined.contains("ccaudit <command> --help"));
}

#[test]
fn version_prints_to_stdout() {
    let h = Harness::new();
    for arg in ["--version", "-V", "version"] {
        let out = h.run(&[arg]);
        require_success(&out, arg);
        let stdout = read_stdout(&out);
        assert!(
            stdout.starts_with("ccaudit "),
            "{arg} should print `ccaudit <ver>` to stdout, got: {stdout:?}"
        );
    }
}

#[test]
fn piped_output_has_no_ansi_escapes() {
    // The headline composability guarantee: when stdout is not a TTY
    // (which it never is under a piped Command), no ANSI escapes leak
    // into the stream, so `> file` / `| grep` get clean text.
    let h = Harness::new();
    setup_single_project(&h);
    for args in [
        vec!["daily"],
        vec!["blocks"],
        vec!["statusline"],
        vec!["session"],
    ] {
        let out = h.run(&args);
        require_success(&out, &format!("{args:?}"));
        let stdout = read_stdout(&out);
        assert!(
            !stdout.contains('\u{1b}'),
            "{args:?} leaked an ANSI escape into a non-TTY pipe:\n{stdout}"
        );
    }
}

#[test]
fn plain_is_tab_separated_without_box_drawing() {
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["daily", "--plain"]);
    require_success(&out, "daily --plain");
    let stdout = read_stdout(&out);
    let first = stdout.lines().next().unwrap_or_default();
    assert!(
        first.starts_with('#'),
        "header should start with #: {first:?}"
    );
    assert!(
        first.contains('\t'),
        "columns should be tab-separated: {first:?}"
    );
    assert!(
        !stdout.contains('│') && !stdout.contains('┌'),
        "--plain must not draw a box:\n{stdout}"
    );
}

#[test]
fn weekly_json_reports_weekly_command() {
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["weekly", "--json"]);
    require_success(&out, "weekly --json");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    assert_eq!(v["command"], "weekly");
}

#[test]
fn web_help_lists_web_only_flags() {
    // All three forms should land on the same web-specific help block.
    let h = Harness::new();
    for args in [
        vec!["web", "--help"],
        vec!["help", "web"],
        vec!["--help", "web"],
    ] {
        let out = h.run(&args);
        require_success(&out, &format!("{args:?}"));
        let combined = format!("{}{}", read_stdout(&out), read_stderr(&out));
        assert!(
            combined.contains("ccaudit web"),
            "missing 'ccaudit web' header for {args:?}: {combined}"
        );
        assert!(
            combined.contains("--port"),
            "missing --port for {args:?}: {combined}"
        );
        assert!(
            combined.contains("--out"),
            "missing --out for {args:?}: {combined}"
        );
    }
}

#[test]
fn tui_help_announces_unfiltered_launch() {
    let h = Harness::new();
    let out = h.run(&["tui", "--help"]);
    require_success(&out, "tui --help");
    let combined = format!("{}{}", read_stdout(&out), read_stderr(&out));
    assert!(combined.contains("ccaudit tui"));
    assert!(combined.contains("not yet honored"));
}

#[test]
fn legacy_double_dash_tui_errors_as_unknown_flag() {
    // Greenfield project: --tui / --web are not parseable. They must hit
    // the unknown-flag arm, not silently dispatch to TUI.
    let h = Harness::new();
    let out = h.run(&["--tui"]);
    let stderr = require_usage_error(&out, "--tui");
    assert!(
        stderr.contains("unknown flag: --tui"),
        "expected unknown-flag error, got: {stderr}"
    );
}

#[test]
fn tui_rejects_global_filters_in_phase_a() {
    // Until tui plumbs the filters through, silently dropping --project is
    // worse than a clear error pointing at the gap.
    let h = Harness::new();
    let out = h.run(&["tui", "--project", "alpha"]);
    let stderr = require_usage_error(&out, "tui --project");
    assert!(
        stderr.contains("--project is not yet honored by `tui`"),
        "expected phase-A rejection, got: {stderr}"
    );
}

#[test]
fn statusline_rejects_report_only_carbon() {
    let h = Harness::new();
    let out = h.run(&["statusline", "--carbon"]);
    let stderr = require_usage_error(&out, "statusline --carbon");
    assert!(
        stderr.contains("--carbon only applies to"),
        "expected report-only rejection, got: {stderr}"
    );
}

#[test]
fn invalid_flag_exits_nonzero() {
    let h = Harness::new();
    let out = h.run(&["--this-flag-does-not-exist"]);
    require_usage_error(&out, "--this-flag-does-not-exist");
}

#[test]
fn source_flag_accepts_claude_aliases() {
    let h = Harness::new();
    setup_single_project(&h);
    for alias in ["claude-code", "claude", "cc"] {
        let out = h.run(&["--source", alias, "--json"]);
        require_success(&out, &format!("--source {alias}"));
        let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
        // Sanity: the report ran and produced rows.
        assert!(v["rows"].as_array().is_some_and(|r| !r.is_empty()));
    }
}

#[test]
fn unknown_source_exits_nonzero() {
    let h = Harness::new();
    let out = h.run(&["--source", "totally-not-a-source"]);
    let stderr = require_usage_error(&out, "--source totally-not-a-source");
    assert!(stderr.contains("unknown source"));
}

#[test]
fn report_title_uses_source_display_name() {
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&[]);
    require_success(&out, "title rendering");
    let stdout = read_stdout(&out);
    assert!(
        stdout.contains("Claude Code Token Usage Report"),
        "expected source display name in title"
    );
}

// refresh-prices: simulate by dropping a minimal LiteLLM-shaped JSON
// into the cache path (avoids network). Then verify that a `--json`
// report picks up the custom rates.
#[test]
fn litellm_prices_override_hardcoded_rates() {
    let h = Harness::new();
    setup_single_project(&h);

    // Write a tiny prices.json mapping our fixture's model to absurdly
    // low rates. If the LiteLLM path is wired in, the total cost will
    // reflect these rates (far below the hardcoded $0.1705).
    let prices = serde_json::json!({
        "claude-opus-4-7": {
            "input_cost_per_token":  0.0000001,
            "output_cost_per_token": 0.0000001,
            "cache_creation_input_token_cost": 0.0000001,
            "cache_read_input_token_cost":     0.0000001
        }
    });
    std::fs::write(
        h.home
            .path()
            .join(".claude")
            .join("ccaudit-cache")
            .join("prices.json"),
        serde_json::to_vec(&prices).unwrap(),
    )
    .unwrap();

    let out = h.run(&["--json"]);
    require_success(&out, "--json with prices override");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    // Fixture totals 15,000 tokens → 0.0015 $ at $0.1 per-million flat.
    //   input 1500 + output 2500 + cw 10000 + cr 1000 = 15_000
    //   × 0.0000001 = 0.0015
    let cost = v["totals"]["cost_usd"].as_f64().unwrap();
    assert!(
        (cost - 0.0015).abs() < 1e-9,
        "expected prices.json to override hardcoded ($0.0015), got {cost}"
    );
}

// ── Env vars ──

#[test]
fn ccaudit_lazy_skips_scan() {
    // Prime the cache with one shape, then add a new session — without
    // --lazy, it shows up; with CCAUDIT_LAZY=1 it's invisible (the scan
    // is skipped and the old cache is used).
    let h = Harness::new();
    setup_single_project(&h);
    let _ = h.run(&[]); // prime cache

    // Add a new project after the cache was written.
    h.write_jsonl(
        "-Users-test-code-gamma",
        "sess_gamma",
        &[
            &summary_line("Gamma"),
            &user_line("hi", "2026-04-03T10:00:00.000Z"),
            &assistant_line(&AssistantLine {
                msg_id: "msg_D",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-03T10:00:01.000Z",
                input: 100,
                output: 100,
                cache_read: 0,
                cache_create: 0,
                text: "gamma reply",
            }),
        ],
    );

    let lazy = h.run_with_env(&["--json"], &[("CCAUDIT_LAZY", "1")]);
    require_success(&lazy, "--json with CCAUDIT_LAZY=1");
    let v: Value = serde_json::from_str(&read_stdout(&lazy)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    // Lazy → uses the original cache, so we see only the first two days.
    assert_eq!(rows.len(), 2);

    // Without lazy, the new day shows up.
    let fresh = h.run(&["--json"]);
    require_success(&fresh, "--json refresh");
    let v: Value = serde_json::from_str(&read_stdout(&fresh)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
}

// ── Regression: hardened arg parsing + filters (2026-07 audit) ──

#[test]
fn timezone_multibyte_offset_is_clean_usage_error() {
    // `+€0` is 4 bytes after the sign with no ':' — a byte-indexed
    // `split_at(2)` lands on a non-char boundary and aborts (SIGABRT,
    // exit 134 under panic=abort). Must be an ordinary usage error.
    let h = Harness::new();
    let out = h.run(&["daily", "--timezone", "+€0"]);
    let stderr = require_usage_error(&out, "--timezone +€0");
    assert!(stderr.contains("bad offset"), "got: {stderr}");
}

#[test]
fn unknown_project_is_rejected() {
    let h = Harness::new();
    setup_two_projects(&h);
    let out = h.run(&["daily", "--json", "--project", "no-such-project"]);
    let stderr = require_usage_error(&out, "--project no-such-project");
    assert!(stderr.contains("unknown project"), "got: {stderr}");
    // Worse than a wrong exit code would be the filter silently
    // disabling itself and printing GLOBAL totals as if they were
    // project-scoped. No report may reach stdout.
    assert!(
        read_stdout(&out).trim().is_empty(),
        "report leaked to stdout despite unknown project:\n{}",
        read_stdout(&out)
    );
}

#[test]
fn project_filter_accepts_stem_display_form() {
    // `ccaudit session` displays ccusage-stem labels ("code-beta"); the
    // label our own output shows must round-trip into --project and
    // select exactly that project, not silently match nothing.
    let h = Harness::new();
    setup_two_projects(&h);

    let out = h.run(&["session", "--json"]);
    require_success(&out, "session --json");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    let labels: Vec<&str> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert!(labels.contains(&"code-beta"), "labels: {labels:?}");

    let out = h.run(&["daily", "--json", "--project", "code-beta"]);
    require_success(&out, "--project code-beta (stem form)");
    let v: Value = serde_json::from_str(&read_stdout(&out)).unwrap();
    // Beta only: 100 in / 200 out — not the global 1600/2700.
    assert_eq!(v["totals"]["input"], 100);
    assert_eq!(v["totals"]["output"], 200);
}

#[test]
fn project_filter_slash_and_stem_forms_agree() {
    let h = Harness::new();
    setup_two_projects(&h);
    let slash = h.run(&["daily", "--json", "--project", "code/beta"]);
    let stem = h.run(&["daily", "--json", "--project", "code-beta"]);
    require_success(&slash, "--project code/beta");
    require_success(&stem, "--project code-beta");
    assert_eq!(
        read_stdout(&slash),
        read_stdout(&stem),
        "stored form and displayed stem form must select the same rows"
    );
}

#[test]
fn value_flag_refuses_flag_shaped_value() {
    // `--project --json` must not swallow `--json` as the project name —
    // that drops the JSON request AND (via the unknown-name hole) the
    // filter. Must be a usage error naming the missing value.
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["daily", "--project", "--json"]);
    let stderr = require_usage_error(&out, "daily --project --json");
    assert!(stderr.contains("missing value"), "got: {stderr}");
    assert!(
        read_stdout(&out).trim().is_empty(),
        "nothing may render on a parse error"
    );
}

#[test]
fn equals_form_matches_space_form() {
    let h = Harness::new();
    setup_single_project(&h);
    let spaced = h.run(&["daily", "--json", "--since", "20260402"]);
    let equals = h.run(&["daily", "--json", "--since=20260402"]);
    require_success(&spaced, "--since 20260402");
    require_success(&equals, "--since=20260402");
    assert_eq!(
        read_stdout(&spaced),
        read_stdout(&equals),
        "`--since=v` must behave exactly like `--since v`"
    );
    // Sanity: the filter really applied (only the 2026-04-02 row).
    let v: Value = serde_json::from_str(&read_stdout(&equals)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "2026-04-02");
}

#[test]
fn since_after_until_is_rejected() {
    // A transposed range must error, not render an empty report with
    // exit 0.
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["daily", "--since", "20260501", "--until", "20260101"]);
    let stderr = require_usage_error(&out, "--since after --until");
    assert!(stderr.contains("range matches nothing"), "got: {stderr}");
}

#[test]
fn blocks_live_and_recent_are_mutually_exclusive() {
    // --live pins the view to the active block, so honoring --recent is
    // impossible — accepting it silently would just drop it.
    let h = Harness::new();
    let out = h.run(&["blocks", "--live", "--recent"]);
    let stderr = require_usage_error(&out, "blocks --live --recent");
    assert!(stderr.contains("mutually exclusive"), "got: {stderr}");
}

#[test]
fn timezone_offset_shifts_day_bucketing() {
    // 23:30Z on Apr 1 is 08:30 on Apr 2 at +09:00 — the row must move to
    // the next local day. A non-UTC offset also forces the per-line
    // (slow) aggregation path, which was previously untested end-to-end.
    let h = Harness::new();
    h.write_jsonl(
        "-Users-test-code-tz",
        "sess_tz",
        &[
            &user_line("late night", "2026-04-01T23:29:00.000Z"),
            &assistant_line(&AssistantLine {
                msg_id: "msg_TZ",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-01T23:30:00.000Z",
                input: 100,
                output: 100,
                cache_read: 0,
                cache_create: 0,
                text: "night owl",
            }),
        ],
    );

    let utc = h.run(&["daily", "--json"]);
    require_success(&utc, "daily UTC");
    let v: Value = serde_json::from_str(&read_stdout(&utc)).unwrap();
    assert_eq!(v["rows"][0]["key"], "2026-04-01", "UTC bucketing");

    let tokyo = h.run(&["daily", "--json", "--timezone", "+09:00"]);
    require_success(&tokyo, "daily +09:00");
    let v: Value = serde_json::from_str(&read_stdout(&tokyo)).unwrap();
    assert_eq!(v["timezone"], "+09:00");
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["key"], "2026-04-02",
        "+09:00 must shift a 23:30Z line to the next local day"
    );
}

#[test]
fn locale_flag_validates_instead_of_silently_ignoring() {
    // `--locale` must never parse-and-ignore: lean builds explain the
    // missing feature, full builds reject unknown locales instead of
    // falling back to POSIX. Tests compile with the same feature set as
    // the binary, so pick the expectation per build.
    let h = Harness::new();
    setup_single_project(&h);
    if cfg!(feature = "locale") {
        let out = h.run(&["daily", "--locale", "klingon"]);
        let stderr = require_usage_error(&out, "--locale klingon");
        assert!(stderr.contains("unknown locale"), "got: {stderr}");

        let out = h.run(&["daily", "--locale", "ja_JP"]);
        require_success(&out, "--locale ja_JP");
    } else {
        let out = h.run(&["daily", "--locale", "ja_JP"]);
        let stderr = require_usage_error(&out, "--locale on lean build");
        assert!(
            stderr.contains("`locale` feature"),
            "lean build must point at the missing feature, got: {stderr}"
        );
    }
}

// ── Flag-list parity ──
//
// ccaudit has no clap dependency: the parser, the typo-hint table, the
// help screens, and the completion scripts each carry their own list of
// flags, and nothing in the type system ties them together. These tests
// do.
//
// This is the one guarantee a derive-based parser gives for free. Buying
// it with a test instead keeps `--help`'s layout fully under our control
// (clap's `{options}` block is not width-configurable) and the
// dependency tree lean, without carrying the drift risk silently.

/// Long flags (sans dashes) named anywhere in the top-level help or in
/// any subcommand's help.
fn advertised_flags(h: &Harness) -> Vec<String> {
    let mut screens = vec![read_stdout(&h.run(&["--help"]))];
    for cmd in [
        "daily",
        "weekly",
        "monthly",
        "session",
        "blocks",
        "statusline",
        "tui",
        "web",
        "refresh-prices",
        "completion",
    ] {
        screens.push(read_stdout(&h.run(&["help", cmd])));
    }
    let mut out: Vec<String> = screens
        .iter()
        .flat_map(|s| s.split_whitespace())
        .filter_map(|w| w.strip_prefix("--"))
        // Trailing punctuation from prose ("--source." / "--out,").
        .map(|w| w.trim_end_matches(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_advertised_flag_is_actually_parsed() {
    let h = Harness::new();
    setup_single_project(&h);
    let flags = advertised_flags(&h);
    assert!(
        flags.len() > 20,
        "help scrape found only {flags:?} — the scraper is broken, not the help"
    );
    for flag in &flags {
        // A flag counts as parsed if any subcommand recognizes it. Scope
        // violations ("--port only applies to web") and missing values
        // both prove the parser knows the spelling; only `unknown flag`
        // means the help advertises something that does not exist.
        let arg = format!("--{flag}");
        let unknown_everywhere = ["daily", "session", "blocks", "statusline", "tui", "web"]
            .iter()
            .all(|cmd| read_stderr(&h.run(&[cmd, &arg])).contains("unknown flag"));
        assert!(
            !unknown_everywhere,
            "help advertises --{flag} but no subcommand's parser accepts it"
        );
    }
}

#[test]
fn completion_offers_every_advertised_flag() {
    let h = Harness::new();
    let advertised = advertised_flags(&h);
    for shell in ["bash", "zsh", "fish"] {
        let script = read_stdout(&h.run(&["completion", shell]));
        for flag in &advertised {
            // `--help` / `--version` are conventionally left out of
            // completion candidate lists; anything else a user can read
            // about, they should be able to tab-complete.
            if flag == "help" || flag == "version" {
                continue;
            }
            assert!(
                script.contains(flag.as_str()),
                "{shell} completion omits --{flag}, which the help advertises"
            );
        }
    }
}

#[test]
fn subcommand_list_is_identical_in_help_and_completion() {
    let h = Harness::new();
    let help = read_stdout(&h.run(&["--help"]));
    let commands: Vec<&str> = help
        .lines()
        .skip_while(|l| !l.starts_with("COMMANDS:"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert!(
        commands.len() > 5,
        "COMMANDS scrape found {commands:?} — the scraper is broken"
    );
    for shell in ["bash", "zsh", "fish"] {
        let script = read_stdout(&h.run(&["completion", shell]));
        for cmd in &commands {
            assert!(
                script.contains(cmd),
                "{shell} completion omits the `{cmd}` subcommand"
            );
        }
    }
}

// ── Cross-source reports ──

/// Give the harness both providers with real, distinct numbers.
/// Claude: 1500 in / 2500 out / 10000 cw / 1000 cr on opus-4-7.
/// Codex:  800 uncached in / 200 cached / 100 out on gpt-5.4.
fn setup_two_providers(h: &Harness) {
    setup_single_project(h);
    let lines = codex_session("cx1", "gpt-5.4", 1000, 200, 100);
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let _ = h.write_codex_rollout(("2026", "04", "01"), "a", &refs);
}

/// `--all` is exactly the sum of the per-provider reports.
///
/// Each provider is aggregated by its own `Source` and only the totals
/// are added, so this also pins that Codex tokens are not silently
/// priced against Claude's rate table — the failure mode that motivated
/// keeping the merge at the rollup level.
#[test]
fn all_sources_sums_each_provider_priced_by_its_own_table() {
    let h = Harness::new();
    setup_two_providers(&h);

    let totals = |args: &[&str]| -> (u64, u64, u64, u64, f64) {
        let out = h.run(args);
        require_success(&out, "totals");
        let v: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
        let t = &v["totals"];
        (
            t["input"].as_u64().unwrap_or(0),
            t["output"].as_u64().unwrap_or(0),
            t["cache_create"].as_u64().unwrap_or(0),
            t["cache_read"].as_u64().unwrap_or(0),
            t["cost_usd"].as_f64().unwrap_or(0.0),
        )
    };

    let claude = totals(&["daily", "--json"]);
    let codex = totals(&["daily", "--json", "--source", "codex"]);
    let all = totals(&["daily", "--json", "--all"]);

    assert_eq!(claude.0, 1500, "claude fixture");
    assert_eq!(codex.0, 800, "codex input excludes the cached portion");
    assert_eq!(codex.3, 200, "codex cached tokens are cache reads");

    assert_eq!(all.0, claude.0 + codex.0, "input");
    assert_eq!(all.1, claude.1 + codex.1, "output");
    assert_eq!(all.2, claude.2 + codex.2, "cache_create");
    assert_eq!(all.3, claude.3 + codex.3, "cache_read");
    assert!(
        (all.4 - (claude.4 + codex.4)).abs() < 1e-9,
        "cost: {} vs {} + {}",
        all.4,
        claude.4,
        codex.4
    );
    // Codex must be priced as Codex: 800@$1.25 + 100@$10 + 200@$0.125.
    assert!(
        (codex.4 - 0.002_025).abs() < 1e-9,
        "codex priced off its own table, got {}",
        codex.4
    );
}

#[test]
fn by_agent_splits_rows_by_provider() {
    let h = Harness::new();
    setup_two_providers(&h);

    let out = h.run(&["daily", "--json", "--by-agent"]);
    require_success(&out, "daily --by-agent");
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let rows = v["rows"].as_array().expect("rows");

    // Both providers were active on 2026-04-01, so that day yields two
    // rows where a plain --all yields one.
    let day1 = rows.iter().filter(|r| r["key"] == "2026-04-01").count();
    assert_eq!(day1, 2, "one row per provider on a shared day: {rows:?}");

    let named: Vec<String> = rows
        .iter()
        .filter_map(|r| r["agent"].as_str().map(str::to_string))
        .collect();
    assert!(
        named.iter().any(|m| m == "Claude Code") && named.iter().any(|m| m == "Codex"),
        "the split dimension must name providers, got {named:?}"
    );
    // A provider is not a model: `.model` must stay absent so a consumer
    // reading it doesn't get "Claude Code" back.
    assert!(
        rows.iter().all(|r| r["model"].is_null()),
        "--by-agent must not populate `model`: {rows:?}"
    );

    // The table's second column names the provider too, so the two
    // renderings agree on what the split dimension is.
    let table = read_stdout(&h.run(&["daily", "--by-agent"]));
    assert!(table.contains("Claude Code"), "table:\n{table}");
    assert!(table.contains("Codex"), "table:\n{table}");
}

#[test]
fn cross_source_flags_reject_what_they_contradict() {
    let h = Harness::new();
    setup_two_providers(&h);

    let e = require_usage_error(
        &h.run(&["daily", "--all", "--source", "codex"]),
        "--all + --source",
    );
    assert!(e.contains("--source"), "got: {e}");

    let e = require_usage_error(
        &h.run(&["daily", "--by-agent", "--breakdown"]),
        "--by-agent + --breakdown",
    );
    assert!(e.contains("--breakdown"), "got: {e}");

    let e = require_usage_error(&h.run(&["tui", "--all"]), "--all on tui");
    assert!(e.contains("--all"), "got: {e}");
}

// ── Config file ──

/// A price override actually reprices, and does so from the RAW model
/// name in the logs rather than the shortened display name.
///
/// Costs are baked into the aggregation cache at build time, so this
/// also pins that editing a rate invalidates it — a stale cache would
/// report the old money while looking perfectly fresh.
#[test]
fn config_price_override_reprices_by_raw_model_name() {
    let h = Harness::new();
    setup_single_project(&h); // 1500 in / 2500 out / 10000 cw / 1000 cr, opus-4-7

    let baseline: Value =
        serde_json::from_slice(&h.run(&["daily", "--json"]).stdout).expect("baseline JSON");
    let base_cost = baseline["totals"]["cost_usd"].as_f64().expect("cost");

    // Ten dollars per million on every column makes the expected total
    // trivially checkable: 15000 tokens → $0.15.
    let cfg = h.write_config(
        "flat.json",
        r#"{"prices":{"claude-opus-4-7":{"input":10.0,"output":10.0,"cache_write":10.0,"cache_read":10.0}}}"#,
    );
    let out = h.run(&["daily", "--json", "--config", &cfg]);
    require_success(&out, "daily --config");
    let v: Value = serde_json::from_slice(&out.stdout).expect("override JSON");
    let cost = v["totals"]["cost_usd"].as_f64().expect("cost");
    assert!(
        (cost - 0.15).abs() < 1e-9,
        "15000 tokens at $10/M should be $0.15, got {cost} (baseline was {base_cost})"
    );

    // The display name is `opus-4-7`; keying on it must NOT match, since
    // it would cover every dated build of that model at once.
    let wrong = h.write_config(
        "display.json",
        r#"{"prices":{"opus-4-7":{"input":10.0,"output":10.0,"cache_write":10.0,"cache_read":10.0}}}"#,
    );
    let v: Value = serde_json::from_slice(&h.run(&["daily", "--json", "--config", &wrong]).stdout)
        .expect("display-name JSON");
    assert!(
        (v["totals"]["cost_usd"].as_f64().expect("cost") - base_cost).abs() < 1e-9,
        "a display-name key must not match; overrides are keyed on the raw log name"
    );
}

#[test]
fn config_supplies_defaults_that_flags_override() {
    let h = Harness::new();
    setup_single_project(&h);

    let cfg = h.write_config("defaults.json", r#"{"no_cost":true}"#);
    let from_cfg = read_stdout(&h.run(&["daily", "--config", &cfg]));
    assert!(!from_cfg.contains('$'), "config `no_cost` was not applied");

    let cfg2 = h.write_config("compact.json", r#"{"compact":true,"timezone":"UTC"}"#);
    let compact = read_stdout(&h.run(&["daily", "--config", &cfg2]));
    let normal = read_stdout(&h.run(&["daily"]));
    let width = |s: &str| s.lines().map(str::len).max().unwrap_or(0);
    assert!(
        width(&compact) < width(&normal),
        "config `compact` was not applied"
    );

    // A flag given alongside the config wins.
    let cfg3 = h.write_config("src.json", r#"{"source":"codex"}"#);
    let out = h.run(&[
        "daily",
        "--json",
        "--config",
        &cfg3,
        "--source",
        "claude-code",
    ]);
    require_success(&out, "flag overriding config source");
    let v: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(
        v["totals"]["input"].as_u64(),
        Some(1500),
        "--source on the command line must beat the config's"
    );
}

#[test]
fn config_errors_are_loud() {
    let h = Harness::new();
    setup_single_project(&h);

    let missing = require_usage_error(
        &h.run(&["daily", "--config", "/definitely/not/here.json"]),
        "missing config",
    );
    assert!(missing.contains("not found"), "got: {missing}");

    // A typo'd key would otherwise leave the user believing an override
    // applied while the shipped price was used.
    let typo = h.write_config("typo.json", r#"{"pricess":{}}"#);
    let err = require_usage_error(&h.run(&["daily", "--config", &typo]), "unknown field");
    assert!(err.contains("unknown field"), "got: {err}");

    let bad_tz = h.write_config("tz.json", r#"{"timezone":"Mars/Olympus"}"#);
    let err = require_usage_error(&h.run(&["daily", "--config", &bad_tz]), "bad timezone");
    assert!(err.contains("timezone"), "got: {err}");
}

#[test]
fn config_sets_log_roots_per_source() {
    let h = Harness::new();
    setup_single_project(&h);
    let line = assistant_line(&AssistantLine {
        msg_id: "m_cfg",
        model: "claude-opus-4-7",
        iso_ts: "2026-05-01T10:00:00.000Z",
        input: 77,
        output: 0,
        cache_read: 0,
        cache_create: 0,
        text: "cfg",
    });
    let _ = h.write_jsonl_at("archive", "-Users-me-code-a", "s", &[&line]);
    let cfg = h.write_config(
        "roots.json",
        &format!(
            r#"{{"sources":{{"claude-code":{{"logs_dirs":["{}"]}}}}}}"#,
            h.root_path("archive")
        ),
    );
    let v: Value = serde_json::from_slice(&h.run(&["daily", "--json", "--config", &cfg]).stdout)
        .expect("JSON");
    assert_eq!(v["totals"]["input"].as_u64(), Some(77));
}

/// `--logs-dir` replaces the provider's default location outright.
///
/// Replacing rather than adding is the point: pointing at an archive
/// should report that archive, not silently blend it with whatever is in
/// `$HOME`. Several roots then compose into one report, so a live
/// directory plus an archive can be read in a single run.
#[test]
fn logs_dir_replaces_the_default_root_and_unions_several() {
    let h = Harness::new();
    // Default location — must NOT appear once --logs-dir is given.
    setup_single_project(&h);

    let alt = |id: &str, input: u64, output: u64| {
        assistant_line(&AssistantLine {
            msg_id: id,
            model: "claude-opus-4-7",
            iso_ts: "2026-05-01T10:00:00.000Z",
            input,
            output,
            cache_read: 0,
            cache_create: 0,
            text: "alt",
        })
    };
    let _ = h.write_jsonl_at("archive-a", "-Users-me-code-a", "s", &[&alt("m_a", 11, 22)]);
    let _ = h.write_jsonl_at("archive-b", "-Users-me-code-b", "s", &[&alt("m_b", 33, 44)]);

    let one = h.run(&["daily", "--json", "--logs-dir", &h.root_path("archive-a")]);
    require_success(&one, "--logs-dir single");
    let v: Value = serde_json::from_slice(&one.stdout).expect("valid JSON");
    assert_eq!(
        v["totals"]["input"].as_u64(),
        Some(11),
        "the default $HOME root must not be scanned once --logs-dir is given"
    );

    let both = format!("{},{}", h.root_path("archive-a"), h.root_path("archive-b"));
    let two = h.run(&["daily", "--json", "--logs-dir", &both]);
    require_success(&two, "--logs-dir comma-separated");
    let v: Value = serde_json::from_slice(&two.stdout).expect("valid JSON");
    assert_eq!(v["totals"]["input"].as_u64(), Some(44), "11 + 33");
    assert_eq!(v["totals"]["output"].as_u64(), Some(66), "22 + 44");
}

#[test]
fn logs_dir_tolerates_a_missing_root() {
    let h = Harness::new();
    let line = assistant_line(&AssistantLine {
        msg_id: "m1",
        model: "claude-opus-4-7",
        iso_ts: "2026-05-01T10:00:00.000Z",
        input: 7,
        output: 9,
        cache_read: 0,
        cache_create: 0,
        text: "x",
    });
    let _ = h.write_jsonl_at("archive-a", "-Users-me-code-a", "s", &[&line]);
    // An archive that has been moved or unmounted shouldn't take the
    // whole report down — the roots that do exist still report.
    let roots = format!("{},{}", h.root_path("archive-a"), h.root_path("gone"));
    let out = h.run(&["daily", "--json", "--logs-dir", &roots]);
    require_success(&out, "--logs-dir with a missing root");
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["totals"]["input"].as_u64(), Some(7));
}

#[test]
fn logs_dir_rejects_an_empty_value() {
    let h = Harness::new();
    let stderr = require_usage_error(&h.run(&["daily", "--logs-dir", ",,"]), "--logs-dir ,,");
    assert!(stderr.contains("--logs-dir"), "got: {stderr}");
}

/// `--no-cost` removes every dollar figure from every output format.
///
/// Dropping the JSON key rather than zeroing it is deliberate: a
/// consumer reading `.cost_usd` should fail loudly instead of quietly
/// believing the run was free.
#[test]
fn no_cost_omits_every_dollar_figure() {
    let h = Harness::new();
    setup_single_project(&h);

    let table = read_stdout(&h.run(&["daily", "--no-cost"]));
    assert!(
        !table.contains("Cost"),
        "table kept a cost header:\n{table}"
    );
    assert!(!table.contains('$'), "table kept a dollar figure:\n{table}");
    assert!(
        !table.contains("Total Prices"),
        "the per-column price row is all dollars and must go too:\n{table}"
    );
    // The box must still close: a suppressed column changes every
    // border and separator, not just the data cells.
    let widths: Vec<usize> = table
        .lines()
        .filter(|l| l.starts_with('┌') || l.starts_with('├') || l.starts_with('└'))
        .map(|l| l.chars().count())
        .collect();
    assert!(!widths.is_empty(), "no table borders found:\n{table}");
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "borders disagree on width after dropping a column: {widths:?}"
    );

    let plain = read_stdout(&h.run(&["daily", "--no-cost", "--plain"]));
    let header = plain.lines().next().unwrap_or_default();
    assert!(!header.contains("cost_usd"), "TSV header: {header}");
    let cols = header.split('\t').count();
    for line in plain.lines().skip(1) {
        assert_eq!(
            line.split('\t').count(),
            cols,
            "TSV row field count must match the header so awk indices hold: {line}"
        );
    }

    let out = h.run(&["daily", "--no-cost", "--json"]);
    require_success(&out, "daily --no-cost --json");
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert!(
        v["totals"].get("cost_usd").is_none(),
        "totals kept cost_usd"
    );
    for row in v["rows"].as_array().expect("rows") {
        assert!(row.get("cost_usd").is_none(), "row kept cost_usd: {row}");
    }
}

#[test]
fn no_cost_rejects_the_flags_it_contradicts() {
    let h = Harness::new();
    setup_single_project(&h);
    let stderr = require_usage_error(
        &h.run(&["blocks", "--no-cost", "--cost-limit", "50"]),
        "both",
    );
    assert!(stderr.contains("--cost-limit"), "got: {stderr}");
}

/// A streamed assistant message bills its FINAL usage, not its first.
///
/// Claude Code rewrites a streamed message as it arrives, repeating the
/// same `message.id` on each line. `input_tokens` and the cache counts
/// are fixed by the request and repeat unchanged; `output_tokens` is a
/// running total that grows. Coalescing to the first line booked a
/// partial output count for every streamed message — 20% of all output
/// on a year of real logs, and invisible because the input and cache
/// columns still looked right.
#[test]
fn streamed_message_bills_its_final_usage() {
    let h = Harness::new();
    // One API call written across three lines, output climbing 100 →
    // 400 → 900 while everything else stays put.
    let usage = |out: u32| {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-04-01T10:00:00.000Z","message":{{"id":"msg_stream","role":"assistant","model":"claude-opus-4-6-20251205","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":1000,"output_tokens":{out},"cache_read_input_tokens":500,"cache_creation_input_tokens":0}}}}}}"#
        )
    };
    let (partial, more, final_) = (usage(100), usage(400), usage(900));
    let _ = h.write_jsonl("-Users-me-code-alpha", "s1", &[&partial, &more, &final_]);

    let out = h.run(&["daily", "--json"]);
    require_success(&out, "daily --json");
    let parsed: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let totals = &parsed["totals"];

    assert_eq!(
        totals["output"].as_u64(),
        Some(900),
        "the run must collapse to its final output count, not its first"
    );
    // The three lines describe one call, so the repeated columns are
    // counted once — not tripled.
    assert_eq!(totals["input"].as_u64(), Some(1000));
    assert_eq!(totals["cache_read"].as_u64(), Some(500));
}

/// `--source` names every provider in the registry, in every screen that
/// mentions one. Adding a `SourceKind` must not leave a help screen
/// advertising a stale list.
#[test]
fn help_names_every_registered_source() {
    let h = Harness::new();
    let ids = ["claude-code", "codex"];
    for screen in [
        read_stdout(&h.run(&["--help"])),
        read_stdout(&h.run(&["help", "tui"])),
        read_stdout(&h.run(&["help", "web"])),
    ] {
        for id in ids {
            assert!(
                screen.contains(id),
                "a --source screen omits the `{id}` provider:\n{screen}"
            );
        }
    }
}
