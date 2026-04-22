// Integration tests for the ccaudit CLI surface.
//
// Every subcommand + every flag exercised against a synthetic fixture.
// Pricing math is deterministic so cost assertions use exact numbers.
//
// Fixture layout per test: one or two projects, each with a single
// session containing assistant lines at known timestamps.
//
//   opus: $5/M in, $25/M out, $10/M cache-write, $0.50/M cache-read
//   Line A: 1000 in + 2000 out + 0 cw + 0 cr   → $0.055
//   Line B: 500 in + 500 out + 1000 cw + 10000 cr → $0.023

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
    // Defensive: a user message containing a literal newline used to
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
    if let Some(&first) = widths.first() {
        for (i, &w) in widths.iter().enumerate() {
            assert_eq!(
                w, first,
                "row {i} width {w} != expected {first}; full output:\n{stdout}"
            );
        }
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
    // commands used to confuse users who expected it to do something;
    // now we reject it up front.
    let h = Harness::new();
    setup_single_project(&h);
    let out = h.run(&["--cost-limit", "1"]);
    assert!(!out.status.success(), "daily --cost-limit should error");
    let out = h.run(&["session", "--cost-limit", "1"]);
    assert!(!out.status.success(), "session --cost-limit should error");
}

#[test]
fn cost_limit_invalid_exits_nonzero() {
    let h = Harness::new();
    let out = h.run(&["blocks", "--cost-limit", "abc"]);
    assert!(!out.status.success());
    let out = h.run(&["blocks", "--cost-limit", "0"]);
    assert!(!out.status.success(), "zero limit should be rejected");
    let out = h.run(&["blocks", "--cost-limit", "-5"]);
    assert!(!out.status.success(), "negative limit should be rejected");
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
    // Help prints to stderr (like most CLIs); check both streams.
    let combined = format!("{}{}", read_stdout(&out), read_stderr(&out));
    assert!(combined.contains("daily"));
    assert!(combined.contains("monthly"));
    assert!(combined.contains("statusline"));
    // The new shape advertises tui/web as real subcommands and a hint to
    // ask each one for its own help.
    assert!(combined.contains("tui"));
    assert!(combined.contains("web"));
    assert!(combined.contains("Run `ccaudit <SUBCOMMAND> --help`"));
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
    assert!(!out.status.success());
    let combined = format!("{}{}", read_stdout(&out), read_stderr(&out));
    assert!(
        combined.contains("unknown flag: --tui"),
        "expected unknown-flag error, got: {combined}"
    );
}

#[test]
fn tui_rejects_global_filters_in_phase_a() {
    // Until tui plumbs the filters through, silently dropping --project is
    // worse than a clear error pointing at the gap.
    let h = Harness::new();
    let out = h.run(&["tui", "--project", "alpha"]);
    assert!(!out.status.success());
    let combined = format!("{}{}", read_stdout(&out), read_stderr(&out));
    assert!(
        combined.contains("--project is not yet honored by `tui`"),
        "expected phase-A rejection, got: {combined}"
    );
}

#[test]
fn statusline_rejects_report_only_carbon() {
    let h = Harness::new();
    let out = h.run(&["statusline", "--carbon"]);
    assert!(!out.status.success());
    let combined = format!("{}{}", read_stdout(&out), read_stderr(&out));
    assert!(
        combined.contains("--carbon only applies to"),
        "expected report-only rejection, got: {combined}"
    );
}

#[test]
fn invalid_flag_exits_nonzero() {
    let h = Harness::new();
    let out = h.run(&["--this-flag-does-not-exist"]);
    assert!(!out.status.success());
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
    assert!(!out.status.success());
    let combined = format!("{}{}", read_stdout(&out), read_stderr(&out));
    assert!(combined.contains("unknown source"));
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
