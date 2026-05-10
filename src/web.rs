use crate::cache::LoadedCache;
use crate::parse::{self, MessageKind, Project};
use crate::source::day_to_date;
use crate::style;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

// What the web app actually consumes per project: just the name
// (display + filter key) and the session list. Per-project totals
// (tokens, cost, msg_count, last_active) are recomputed in JS from
// sessions at render time so date / model filters can shrink them
// — shipping pre-aggregated copies would lie under any active filter.
#[derive(Serialize)]
struct IndexProject<'a> {
    name: &'a str,
    sessions: Vec<IndexSession<'a>>,
}

#[derive(Serialize)]
struct IndexSession<'a> {
    id: &'a str,
    summary: Option<&'a str>,
    first_user_msg: Option<&'a str>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    // Pre-rendered rfc3339 strings — chrono can't hand out a borrowed
    // representation, so these necessarily allocate. Kept owned.
    started_at: Option<String>,
    ended_at: Option<String>,
    turn_count: usize,
    model: Option<&'a str>,
    msg_count: usize,
    cost: f64,
    // Per-token-type dollar costs. Sum to `cost`. Shipped so the
    // web's cost-cell hover can show the real breakdown without
    // re-pricing on the JS side.
    cost_input: f64,
    cost_output: f64,
    cost_cache_read: f64,
    cost_cache_create: f64,
    file: String,
    /// Per-hour token aggregates, each: `[unix_hour_ts, in, out, cr, cw]`.
    /// Built from real per-message timestamps so the hour histogram
    /// doesn't have to guess where within a session the tokens landed.
    /// Compact array-of-arrays to keep JSON small.
    hourly: Vec<[u64; 5]>,
    /// Count of `ToolUse` invocations by `tool_name`, aggregated across
    /// this session. Powers the dashboard pie chart's `by tool` mode.
    /// Small map — each session touches maybe 5–20 distinct tools.
    tool_counts: FxHashMap<String, u32>,
}

fn build_tool_counts_from(messages: &[parse::Message]) -> FxHashMap<String, u32> {
    let mut map: FxHashMap<String, u32> =
        FxHashMap::with_capacity_and_hasher(16, rustc_hash::FxBuildHasher);
    for msg in messages {
        if matches!(msg.kind, MessageKind::ToolUse) {
            if let Some(name) = msg.tool_name.as_deref() {
                *map.entry(name.to_owned()).or_insert(0) += 1;
            }
        }
    }
    map
}

// Render the shared design tokens into a CSS :root block. Emitted
// once into the generated stylesheet so both ccaudit and a hand-written
// stylesheet would reference the same variables.
fn build_css_tokens() -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(1024);
    s.push_str(":root{\n");
    let rows: &[(&str, style::Rgb)] = &[
        // Core palette
        ("--bg", style::BG),
        ("--bg2", style::BG2),
        ("--bg3", style::BG3),
        ("--fg", style::FG),
        ("--fg2", style::FG2),
        ("--fg3", style::FG3),
        ("--accent", style::ACCENT),
        ("--cyan", style::CYAN),
        ("--green", style::GREEN),
        ("--yellow", style::YELLOW),
        ("--magenta", style::MAGENTA),
        ("--red", style::RED),
        ("--border", style::BORDER),
        // Semantic kind tokens
        ("--k-user", style::K_USER),
        ("--k-assistant", style::K_ASSISTANT),
        ("--k-tooluse", style::K_TOOLUSE),
        ("--k-toolresult", style::K_TOOLRESULT),
        ("--k-thinking", style::K_THINKING),
        ("--k-system", style::K_SYSTEM),
    ];
    for (name, rgb) in rows {
        let _ = writeln!(s, "  {name}:{};", style::css_hex(*rgb));
    }
    s.push_str("}\n");
    s
}

fn build_hourly_from(messages: &[parse::Message]) -> Vec<[u64; 5]> {
    let mut map: FxHashMap<u64, (u64, u64, u64, u64)> = FxHashMap::default();
    for msg in messages {
        let Some(ts) = msg.timestamp else { continue };
        let Some(tok) = msg.tokens.as_ref() else {
            continue;
        };
        let hour_ts = (ts.timestamp().max(0) as u64 / 3600) * 3600;
        let entry = map.entry(hour_ts).or_default();
        entry.0 += tok.input;
        entry.1 += tok.output;
        entry.2 += tok.cache_read;
        entry.3 += tok.cache_create;
    }
    let mut out: Vec<[u64; 5]> = map
        .into_iter()
        .map(|(t, (i, o, cr, cw))| [t, i, o, cr, cw])
        .collect();
    out.sort_unstable_by_key(|row| row[0]);
    out
}

/// Compact daily rollup shipped alongside `index.json`. Derived from
/// `cache::aggregate(.., Bucket::Day, ..)` — the exact same code path
/// that powers `ccaudit daily`, so the heatmap and the CLI usage table
/// agree to the cent.
///
/// `rows` are flat `[day_idx, project_idx, model_idx, input, output,
/// cache_read, cache_create, cost]`. Strings are interned into the top
/// `p` / `m` / `d` arrays so a year of history stays under ~100 KB.
#[derive(Serialize)]
struct DailyIndex<'a> {
    p: &'a [String], // project names (index space matches `rows[_][1]`)
    m: &'a [String], // model names — empty string for unknown
    d: Vec<String>,  // day strings in YYYY-MM-DD form
    rows: Vec<DailyRow>,
}

// `serde_json` serializes `(i32, i32, i32, u64, u64, u64, u64, f64)` as
// a heterogeneous JSON array — matches the compact wire format the
// `hourly` field already uses.
#[derive(Serialize)]
struct DailyRow(i32, i32, i32, u64, u64, u64, u64, f64);

fn build_daily_index(cache: &LoadedCache) -> DailyIndex<'_> {
    // Preaggs are already cross-session-deduped and priced per-message
    // model at build time, keyed on (day, model_id, project_id). So
    // the "daily" rollup is just a projection: group by (day, project,
    // model) and collect ordered indices for each string dimension.
    let mut day_idx: HashMap<i32, i32> = HashMap::new();
    let mut days: Vec<i32> = Vec::new();

    let mut rows: Vec<DailyRow> = Vec::with_capacity(cache.preaggs().len());
    for p in cache.preaggs() {
        let di = *day_idx.entry(p.day).or_insert_with(|| {
            let i = i32::try_from(days.len()).unwrap_or(i32::MAX);
            days.push(p.day);
            i
        });
        // project_id == u16::MAX is reserved for providers without a
        // project concept; Claude Code always sets one, but stay defensive.
        let pi = if (p.project_id as usize) < cache.projects.len() {
            i32::from(p.project_id)
        } else {
            -1
        };
        let mi = if p.model_id == u16::MAX {
            -1
        } else {
            i32::from(p.model_id)
        };
        rows.push(DailyRow(
            di,
            pi,
            mi,
            u64::from(p.input),
            u64::from(p.output),
            u64::from(p.cache_read),
            u64::from(p.cache_create),
            p.total_cost(),
        ));
    }

    // Day strings are small but numerous — render once here rather than
    // letting the JS do it per cell. Same YYYY-MM-DD form the existing
    // heatmap keys on, so the lookup stays O(1).
    let d: Vec<String> = days
        .iter()
        .map(|&d| day_to_date(d).format("%Y-%m-%d").to_string())
        .collect();

    DailyIndex {
        p: &cache.projects,
        m: &cache.models,
        d,
        rows,
    }
}

// Prints build-stats lines ("search index: …KB", "wrote …") to stderr
// so the user sees what the `ccaudit web` run produced.
#[allow(clippy::print_stderr)]
pub fn generate(projects: &[Project], cache: &LoadedCache, out_dir: &Path) -> std::io::Result<()> {
    use rayon::prelude::*;

    let sessions_dir = out_dir.join("s");
    fs::create_dir_all(&sessions_dir)?;

    // Collect all (pi, si, session) tuples for parallel processing
    let all_sessions: Vec<(usize, usize, &parse::Session)> = projects
        .iter()
        .enumerate()
        .flat_map(|(pi, p)| {
            p.sessions
                .iter()
                .enumerate()
                .map(move |(si, s)| (pi, si, s))
        })
        .collect();

    // Parallel: load each session's `.msgs` blob once, derive everything
    // that needs message content (tokenized search words, hourly
    // breakdown, tool-use histogram), stream the per-session JSON to
    // disk, then drop the messages. The session-list path no longer
    // keeps message content in memory, so this is the only place that
    // touches `.msgs` during web generate.
    struct PerSession {
        words: FxHashSet<String>,
        hourly: Vec<[u64; 5]>,
        tool_counts: FxHashMap<String, u32>,
    }
    let per_session: Vec<PerSession> = all_sessions
        .par_iter()
        .map(|&(pi, si, session)| {
            let messages: Vec<parse::Message> = parse::load_messages_for(&session.file_path)
                .or_else(|| parse::parse_session(&session.file_path).map(|s| s.messages))
                .unwrap_or_default();
            let filename = format!("{pi}_{si}.json");
            if let Ok(file) = fs::File::create(sessions_dir.join(&filename)) {
                let mut w = BufWriter::new(file);
                let _ = serde_json::to_writer(&mut w, &messages);
                let _ = w.flush();
            }
            let mut words: FxHashSet<String> = FxHashSet::default();
            for msg in &messages {
                match msg.kind {
                    MessageKind::User | MessageKind::Assistant | MessageKind::ToolUse => {
                        tokenize_into(&msg.content, &mut words);
                    }
                    _ => {}
                }
            }
            PerSession {
                words,
                hourly: build_hourly_from(&messages),
                tool_counts: build_tool_counts_from(&messages),
            }
        })
        .collect();

    // Build index sequentially. `per_session` is in flat (pi,si) order
    // matching `all_sessions`, so we pop the front of an iterator as we
    // re-walk the projects tree — moves hourly/tool_counts into the
    // IndexSession instead of cloning.
    let mut per_session_iter = per_session.into_iter();
    let mut per_session_words: Vec<FxHashSet<String>> = Vec::with_capacity(all_sessions.len());
    let index: Vec<IndexProject> = projects
        .iter()
        .enumerate()
        .map(|(pi, project)| {
            let idx_sessions: Vec<IndexSession> = project
                .sessions
                .iter()
                .enumerate()
                .map(|(si, session)| {
                    let filename = format!("{pi}_{si}.json");
                    let cost = session.cost;
                    let ps = per_session_iter.next().unwrap_or(PerSession {
                        words: FxHashSet::default(),
                        hourly: Vec::new(),
                        tool_counts: FxHashMap::default(),
                    });
                    per_session_words.push(ps.words);
                    IndexSession {
                        id: &session.id,
                        summary: session.summary.as_deref(),
                        first_user_msg: session.first_user_msg.as_deref(),
                        total_input_tokens: session.total_input_tokens,
                        total_output_tokens: session.total_output_tokens,
                        total_cache_read: session.total_cache_read,
                        total_cache_create: session.total_cache_create,
                        started_at: session.started_at.map(|t| t.to_rfc3339()),
                        ended_at: session.ended_at.map(|t| t.to_rfc3339()),
                        turn_count: session.turn_count,
                        model: session.model.as_deref(),
                        msg_count: session.msg_count as usize,
                        cost,
                        cost_input: session.cost_input,
                        cost_output: session.cost_output,
                        cost_cache_read: session.cost_cache_read,
                        cost_cache_create: session.cost_cache_create,
                        file: filename,
                        hourly: ps.hourly,
                        tool_counts: ps.tool_counts,
                    }
                })
                .collect();

            IndexProject {
                name: &project.name,
                sessions: idx_sessions,
            }
        })
        .collect();

    // per_session_words is ordered same as all_sessions (flat project→session iteration)
    // Drain each set so we move the strings into the posting-list map
    // instead of cloning them. Pre-size to the total set-element count
    // (overshoots a bit since popular words appear in many sessions, but
    // avoids the rehash cascade of growing from default capacity).
    let total_word_slots: usize = per_session_words.iter().map(FxHashSet::len).sum();
    let mut word_to_sessions: FxHashMap<String, Vec<usize>> =
        FxHashMap::with_capacity_and_hasher(total_word_slots, rustc_hash::FxBuildHasher);
    for (flat_idx, words) in per_session_words.into_iter().enumerate() {
        for word in words {
            word_to_sessions.entry(word).or_default().push(flat_idx);
        }
    }

    // Single top-level document for the web client: `projects` is the
    // existing session/message tree; `daily` is derived from the shared
    // aggregation cache (same substrate as `ccaudit daily`) so the
    // heatmap can't drift from the CLI numbers. One fetch, one HTTP
    // cache entry, one source of truth.
    #[derive(Serialize)]
    struct Index<'a> {
        projects: &'a [IndexProject<'a>],
        daily: DailyIndex<'a>,
    }
    let daily = build_daily_index(cache);
    let index_doc = Index {
        projects: &index,
        daily,
    };
    let index_path = out_dir.join("index.json");
    let mut index_w = BufWriter::new(fs::File::create(&index_path)?);
    serde_json::to_writer(&mut index_w, &index_doc).map_err(std::io::Error::other)?;
    index_w.flush()?;
    drop(index_w);
    let index_size = fs::metadata(&index_path).map(|m| m.len()).unwrap_or(0);

    #[derive(Serialize)]
    struct SearchIndex {
        w: FxHashMap<String, Vec<usize>>,
    }
    let search_path = out_dir.join("search.json");
    let mut search_w = BufWriter::new(fs::File::create(&search_path)?);
    serde_json::to_writer(
        &mut search_w,
        &SearchIndex {
            w: word_to_sessions,
        },
    )
    .map_err(std::io::Error::other)?;
    search_w.flush()?;
    drop(search_w);
    let search_size = fs::metadata(&search_path).map(|m| m.len()).unwrap_or(0);
    eprintln!("search index: {:.0}KB", search_size as f64 / 1024.0);

    let out_file = out_dir.join("index.html");
    let mut f = BufWriter::new(fs::File::create(&out_file)?);
    f.write_all(b"<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<meta name=\"color-scheme\" content=\"dark\">\n<meta name=\"description\" content=\"Browse Claude Code session logs - projects, token usage, costs, and full message history.\">\n<meta property=\"og:title\" content=\"ccaudit\">\n<meta property=\"og:description\" content=\"Claude Code session log browser.\">\n<meta property=\"og:type\" content=\"website\">\n<title>ccaudit</title>\n<link rel=\"icon\" href=\"data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><rect width='32' height='32' rx='6' fill='%230d0d0f'/><text x='16' y='22' font-size='18' text-anchor='middle' fill='%236e9eff' font-family='monospace'>cc</text></svg>\">\n<style>")?;
    // Swap the `/* TOKENS */` placeholder with a :root block generated
    // from the shared design tokens. One source of truth (style.rs).
    // Spaces around `TOKENS` are required — prettier normalizes the
    // marker to that form, so the literal here must match. The
    // assert below catches a refactor that drops/renames the marker;
    // without it the design-token block silently disappears.
    debug_assert!(
        CSS.contains(TOKEN_MARKER),
        "src/web/style.css must contain `{TOKEN_MARKER}` so the build can splice in the design tokens",
    );
    let css = CSS.replacen(TOKEN_MARKER, &build_css_tokens(), 1);
    f.write_all(css.as_bytes())?;
    f.write_all(b"</style>\n</head>\n<body>\n<div id=\"narrow\" role=\"alert\"><strong>ccaudit is desktop-only</strong>The views are dense and table-heavy \xe2\x80\x94 they need a wider viewport than this device offers. Open ccaudit on a laptop or desktop browser.</div>\n<div id=\"app\">\n  <header>\n    <div class=\"bar\">\n      <button id=\"back\" onclick=\"goBack()\" class=\"hidden\" aria-label=\"back\">&larr;</button>\n      <nav id=\"crumbs\" class=\"crumbs\" aria-label=\"breadcrumb\">\n        <span class=\"crumb-lbl\">project:</span><a id=\"crumb-p\" class=\"crumb dim\" onclick=\"crumbClickP()\" role=\"button\" tabindex=\"0\">\xe2\x80\x94</a>\n        <span class=\"crumb-sep\" aria-hidden=\"true\">/</span>\n        <span class=\"crumb-lbl\">session:</span><a id=\"crumb-s\" class=\"crumb dim\" onclick=\"crumbClickS()\" role=\"button\" tabindex=\"0\">\xe2\x80\x94</a>\n      </nav>\n      <div class=\"filterset\" role=\"toolbar\" aria-label=\"filters\">\n        <input id=\"search\" type=\"search\" placeholder=\"/ search\" autocomplete=\"off\" spellcheck=\"false\" aria-label=\"search\">\n        <button class=\"pbtn reset\" onclick=\"resetAll()\" title=\"clear all filters / sort / scope (r)\" aria-label=\"reset filters\">reset</button>\n        <input id=\"dfrom\" type=\"date\" class=\"dateinp\" title=\"from date\" aria-label=\"from date\">\n        <input id=\"dto\" type=\"date\" class=\"dateinp\" title=\"to date\" aria-label=\"to date\">\n        <div class=\"presets\" role=\"group\" aria-label=\"date preset\">\n          <button class=\"pbtn\" data-days=\"7\" onclick=\"setDateRange(7)\">7d</button>\n          <button class=\"pbtn\" data-days=\"30\" onclick=\"setDateRange(30)\">30d</button>\n          <button class=\"pbtn\" data-days=\"90\" onclick=\"setDateRange(90)\">90d</button>\n          <button class=\"pbtn\" data-days=\"0\" onclick=\"setDateRange(null)\">all</button>\n        </div>\n        <div id=\"mfilt\" class=\"drop\" data-drop=\"model\" title=\"filter by model\"></div>\n      </div>\n    </div>\n  </header>\n  <main id=\"main\" role=\"main\"><div class=\"loading\" role=\"status\" aria-live=\"polite\">loading...</div></main>\n  <button id=\"btt\" onclick=\"document.getElementById('main').scrollTo({top:0,behavior:'smooth'})\" title=\"back to top\" aria-label=\"back to top\">\xe2\x86\x91</button>\n</div>\n<script>\n")?;
    f.write_all(UTIL.as_bytes())?;
    f.write_all(JS.as_bytes())?;
    f.write_all(b"\n</script>\n</body>\n</html>")?;
    f.flush()?;

    let total_session_files: usize = projects.iter().map(|p| p.sessions.len()).sum();
    eprintln!(
        "wrote {} ({:.0}KB index + {} session files)",
        out_file.display(),
        index_size as f64 / 1024.0,
        total_session_files
    );
    Ok(())
}

fn tokenize_into(text: &str, out: &mut FxHashSet<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find start of token: must begin with ascii alpha
        if bytes[i].is_ascii_alphabetic() {
            let start = i;
            i += 1;
            // Continue with alphanumeric or underscore
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let len = i - start;
            if len >= 3 {
                // Safe: we only matched ASCII bytes
                let word = text[start..i].to_ascii_lowercase();
                let _ = out.insert(word);
            }
        } else {
            i += 1;
        }
    }
}

const CSS: &str = include_str!("web/style.css");
const TOKEN_MARKER: &str = "/* TOKENS */";

// Pure helpers (no DOM, no state). Prepended to JS so app.js can treat
// these functions as globals. Also loadable standalone for testing —
// see `src/web/test.html`.
const UTIL: &str = include_str!("web/util.js");
const JS: &str = include_str!("web/app.js");
