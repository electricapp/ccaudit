use crate::parse::{self, MessageKind, Project};
use crate::style;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Serialize)]
struct IndexProject<'a> {
    name: &'a str,
    total_tokens: u64,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    msg_count: usize,
    cost: f64,
    last_active: Option<String>,
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
    tool_counts: HashMap<&'a str, u32>,
}

fn build_tool_counts(session: &parse::Session) -> HashMap<&str, u32> {
    let mut map: HashMap<&str, u32> = HashMap::new();
    for msg in &session.messages {
        if matches!(msg.kind, MessageKind::ToolUse) {
            if let Some(name) = msg.tool_name.as_deref() {
                *map.entry(name).or_insert(0) += 1;
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

fn build_hourly(session: &parse::Session) -> Vec<[u64; 5]> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<u64, (u64, u64, u64, u64)> = BTreeMap::new();
    for msg in &session.messages {
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
    map.into_iter()
        .map(|(t, (i, o, cr, cw))| [t, i, o, cr, cw])
        .collect()
}

pub fn generate(projects: &[Project], out_dir: &Path) -> std::io::Result<()> {
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

    // Parallel: serialize + write session JSON files AND tokenize for search index
    let per_session_words: Vec<HashSet<String>> = all_sessions
        .par_iter()
        .map(|&(pi, si, session)| {
            let filename = format!("{pi}_{si}.json");
            if let Ok(json) = serde_json::to_string(&session.messages) {
                let _ = fs::write(sessions_dir.join(&filename), json);
            }
            let mut words: HashSet<String> = HashSet::new();
            for msg in &session.messages {
                match msg.kind {
                    MessageKind::User | MessageKind::Assistant | MessageKind::ToolUse => {
                        tokenize_into(&msg.content, &mut words);
                    }
                    _ => {}
                }
            }
            words
        })
        .collect();

    // Build index + search index sequentially (fast, just metadata)
    let index: Vec<IndexProject> = projects
        .iter()
        .enumerate()
        .map(|(pi, project)| {
            let total_input: u64 = project.sessions.iter().map(|s| s.total_input_tokens).sum();
            let total_output: u64 = project.sessions.iter().map(|s| s.total_output_tokens).sum();
            let total_cache_read: u64 = project.sessions.iter().map(|s| s.total_cache_read).sum();
            let total_cache_create: u64 =
                project.sessions.iter().map(|s| s.total_cache_create).sum();
            let msg_count: usize = project.sessions.iter().map(|s| s.messages.len()).sum();

            let idx_sessions: Vec<IndexSession> = project
                .sessions
                .iter()
                .enumerate()
                .map(|(si, session)| {
                    let filename = format!("{pi}_{si}.json");
                    let cost = session.cost;
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
                        msg_count: session.messages.len(),
                        cost,
                        cost_input: session.cost_input,
                        cost_output: session.cost_output,
                        cost_cache_read: session.cost_cache_read,
                        cost_cache_create: session.cost_cache_create,
                        file: filename,
                        hourly: build_hourly(session),
                        tool_counts: build_tool_counts(session),
                    }
                })
                .collect();

            let cost: f64 = idx_sessions.iter().map(|s| s.cost).sum();
            IndexProject {
                name: &project.name,
                total_tokens: project.total_tokens,
                total_input,
                total_output,
                total_cache_read,
                total_cache_create,
                msg_count,
                cost,
                last_active: project.last_active.map(|t| t.to_rfc3339()),
                sessions: idx_sessions,
            }
        })
        .collect();

    // per_session_words is ordered same as all_sessions (flat project→session iteration)
    // Drain each HashSet so we move the strings into the posting-list
    // map instead of cloning them.
    let mut word_to_sessions: HashMap<String, Vec<usize>> = HashMap::new();
    for (flat_idx, words) in per_session_words.into_iter().enumerate() {
        for word in words {
            word_to_sessions.entry(word).or_default().push(flat_idx);
        }
    }

    let index_json = serde_json::to_string(&index).map_err(std::io::Error::other)?;
    fs::write(out_dir.join("index.json"), &index_json)?;

    #[derive(Serialize)]
    struct SearchIndex {
        w: HashMap<String, Vec<usize>>,
    }
    let search_json = serde_json::to_string(&SearchIndex {
        w: word_to_sessions,
    })
    .map_err(std::io::Error::other)?;
    fs::write(out_dir.join("search.json"), &search_json)?;
    eprintln!("search index: {:.0}KB", search_json.len() as f64 / 1024.0);

    let out_file = out_dir.join("index.html");
    let mut f = fs::File::create(&out_file)?;
    f.write_all(b"<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<meta name=\"color-scheme\" content=\"dark\">\n<meta name=\"description\" content=\"Browse Claude Code session logs - projects, token usage, costs, and full message history.\">\n<meta property=\"og:title\" content=\"ccaudit\">\n<meta property=\"og:description\" content=\"Claude Code session log browser.\">\n<meta property=\"og:type\" content=\"website\">\n<title>ccaudit</title>\n<link rel=\"icon\" href=\"data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><rect width='32' height='32' rx='6' fill='%230d0d0f'/><text x='16' y='22' font-size='18' text-anchor='middle' fill='%236e9eff' font-family='monospace'>cc</text></svg>\">\n<style>")?;
    // Swap the `/* TOKENS */` placeholder with a :root block generated
    // from the shared design tokens. One source of truth (style.rs).
    // Spaces around `TOKENS` are required — prettier normalizes the
    // marker to that form, so the literal here must match.
    let css = CSS.replacen("/* TOKENS */", &build_css_tokens(), 1);
    f.write_all(css.as_bytes())?;
    f.write_all(b"</style>\n</head>\n<body>\n<div id=\"narrow\" role=\"alert\"><strong>ccaudit is desktop-only</strong>The views are dense and table-heavy \xe2\x80\x94 they need a wider viewport than this device offers. Open ccaudit on a laptop or desktop browser.</div>\n<div id=\"app\">\n  <header>\n    <div class=\"bar\">\n      <button id=\"back\" onclick=\"goBack()\" class=\"hidden\" aria-label=\"back\">&larr;</button>\n      <nav id=\"crumbs\" class=\"crumbs\" aria-label=\"breadcrumb\">\n        <span class=\"crumb-lbl\">project:</span><a id=\"crumb-p\" class=\"crumb dim\" onclick=\"crumbClickP()\" role=\"button\" tabindex=\"0\">\xe2\x80\x94</a>\n        <span class=\"crumb-sep\" aria-hidden=\"true\">/</span>\n        <span class=\"crumb-lbl\">session:</span><a id=\"crumb-s\" class=\"crumb dim\" onclick=\"crumbClickS()\" role=\"button\" tabindex=\"0\">\xe2\x80\x94</a>\n      </nav>\n      <div class=\"filterset\" role=\"toolbar\" aria-label=\"filters\">\n        <input id=\"search\" type=\"search\" placeholder=\"/ search\" autocomplete=\"off\" spellcheck=\"false\" aria-label=\"search\">\n        <button class=\"pbtn reset\" onclick=\"resetAll()\" title=\"clear all filters / sort / scope (r)\" aria-label=\"reset filters\">reset</button>\n        <input id=\"dfrom\" type=\"date\" class=\"dateinp\" title=\"from date\" aria-label=\"from date\">\n        <input id=\"dto\" type=\"date\" class=\"dateinp\" title=\"to date\" aria-label=\"to date\">\n        <div class=\"presets\" role=\"group\" aria-label=\"date preset\">\n          <button class=\"pbtn\" data-days=\"7\" onclick=\"setDateRange(7)\">7d</button>\n          <button class=\"pbtn\" data-days=\"30\" onclick=\"setDateRange(30)\">30d</button>\n          <button class=\"pbtn\" data-days=\"90\" onclick=\"setDateRange(90)\">90d</button>\n          <button class=\"pbtn\" data-days=\"0\" onclick=\"setDateRange(null)\">all</button>\n        </div>\n        <div id=\"mfilt\" class=\"drop\" data-drop=\"model\" title=\"filter by model\"></div>\n      </div>\n    </div>\n  </header>\n  <main id=\"main\" role=\"main\"><div class=\"loading\" role=\"status\" aria-live=\"polite\">loading...</div></main>\n  <button id=\"btt\" onclick=\"document.getElementById('main').scrollTo({top:0,behavior:'smooth'})\" title=\"back to top\" aria-label=\"back to top\">\xe2\x86\x91</button>\n</div>\n<script>\n")?;
    f.write_all(UTIL.as_bytes())?;
    f.write_all(JS.as_bytes())?;
    f.write_all(b"\n</script>\n</body>\n</html>")?;

    let index_size = index_json.len();
    let total_session_files: usize = projects.iter().map(|p| p.sessions.len()).sum();
    eprintln!(
        "wrote {} ({:.0}KB index + {} session files)",
        out_file.display(),
        index_size as f64 / 1024.0,
        total_session_files
    );
    Ok(())
}

fn tokenize_into(text: &str, out: &mut HashSet<String>) {
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

// Pure helpers (no DOM, no state). Prepended to JS so app.js can treat
// these functions as globals. Also loadable standalone for testing —
// see `src/web/test.html`.
const UTIL: &str = include_str!("web/util.js");
const JS: &str = include_str!("web/app.js");
