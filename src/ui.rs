use crate::keymap::{Action as Act, Key, Name};
use crate::parse::{self, MessageKind, Project};
use crate::report::fmt::{format_cost, format_datetime, format_datetime_short, format_number};
use crate::search::Searcher;
use crate::style;
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::time::Duration;

// Pre-allocated spaces slab; slice out any indent width up to 64 chars
// with no runtime allocation. Used by build_message_lines when the
// message line cache is (re)built; wider pads fall back to allocation.
const SPACES: &str = "                                                                ";

/// crossterm event -> the keymap's key type.
const fn to_key(k: event::KeyEvent) -> Option<Key> {
    let name = match k.code {
        KeyCode::Char(c) => Name::Char(c),
        KeyCode::Tab => Name::Tab,
        KeyCode::Enter => Name::Enter,
        KeyCode::Esc => Name::Esc,
        KeyCode::Up => Name::Up,
        KeyCode::Down => Name::Down,
        KeyCode::Left => Name::Left,
        KeyCode::Right => Name::Right,
        KeyCode::PageUp => Name::PageUp,
        KeyCode::PageDown => Name::PageDown,
        KeyCode::Home => Name::Home,
        KeyCode::End => Name::End,
        KeyCode::Backspace => Name::Backspace,
        _ => return None,
    };
    Some(Key {
        name,
        ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
    })
}

#[derive(PartialEq)]
enum View {
    Projects,
    Sessions,
    Messages,
    Dashboard,
}

/// Hand-off request from the TUI to another program.
///
/// Set when the user asks the TUI to launch something else (e.g. `c` →
/// resume a Claude Code session, `o` → launch the web view). The TUI
/// quits cleanly and `main.rs` runs the action after terminal teardown,
/// so the new process inherits a clean stdio.
pub enum PostAction {
    /// Resume `id` in Claude Code from `cwd` — `claude -r` resolves against
    /// the project derived from the current directory.
    Resume {
        id: String,
        cwd: Option<String>,
    },
    OpenWeb,
}

// Independent one-off UI flags (search mode, detail pane, quit,
// deferred load) — a state enum would force invalid combinations apart
// that are genuinely orthogonal here.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    projects: Vec<Project>,
    /// Provider whose logs are on screen. Held so a session opened from
    /// the list re-reads its transcript through the same parser that
    /// built the list, rather than assuming Claude Code's log shape.
    source: &'static dyn crate::source::Source,
    searcher: Searcher,
    view: View,
    search_query: String,
    searching: bool,
    project_state: ListState,
    session_state: ListState,
    message_scroll: u16,
    message_max_scroll: u16,
    // Rendered lines for the open session, keyed on (project, session,
    // width). Message content is immutable once loaded, so rebuilding
    // the whole buffer per keystroke would be O(session content) for a
    // viewport of ~one screen; a resize changes the width key and
    // forces a rebuild.
    message_lines: Vec<Line<'static>>,
    message_lines_key: Option<(usize, usize, u16)>,
    // Set when Enter opens a session whose messages aren't in memory.
    // The load can block (cache-miss → full JSONL parse), so `run`
    // paints one "loading" frame first, then loads and repaints —
    // loading inside the key handler freezes the UI with no feedback.
    pending_load: bool,
    selected_project: Option<usize>,
    selected_session: Option<usize>,
    filtered_projects: Vec<usize>,
    filtered_sessions: Vec<usize>,
    dash_scroll: u16,
    dash_max_scroll: u16,
    session_detail_open: bool,
    /// `?` overlay. The status bar only has room for the current view's
    /// bindings, so the full reference lives behind a key — same set the
    /// web's `?` shows, so the two surfaces document one keymap.
    keys_open: bool,
    /// Resolved bindings, config `[keys]` applied over the defaults.
    keys: crate::keymap::Keymap,
    /// Every action by name, for a key you don't know or rebound.
    palette_open: bool,
    palette_query: String,
    palette_sel: usize,
    /// Config problems worth showing once, in the status bar.
    warnings: Vec<String>,
    pub post_action: Option<PostAction>,
    quit: bool,
    // Dashboard aggregations computed once at App::new(). Projects are
    // immutable for the TUI's lifetime, so recomputing per-frame was
    // pure waste (FxHashMap<String,_> allocating a fresh key per session
    // on every keystroke).
    dashboard: DashboardAgg,
}

struct DashboardAgg {
    total_sessions: usize,
    total_msgs: usize,
    total_cost: f64,
    total_input: u64,
    total_output: u64,
    total_cache_w: u64,
    total_cache_r: u64,
    by_project: Vec<ProjectAgg>,
    by_model: Vec<ModelAgg>,
    // Top 50 sessions by cost, sorted desc. 50 gives headroom over the
    // 10 currently displayed in case we later expose scrolling.
    top_sessions: Vec<TopSession>,
}

struct ProjectAgg {
    idx: usize,
    sess_count: usize,
    msg_count: usize,
    tokens: u64,
    cost: f64,
    duration_ms: u64,
    last_active: String,
}

struct ModelAgg {
    name: String,
    sess_count: usize,
    msg_count: usize,
    tokens: u64,
    cost: f64,
    duration_ms: u64,
    last_active: String,
}

struct TopSession {
    pi: usize,
    si: usize,
    cost: f64,
    msgs: usize,
    tokens: u64,
    duration_ms: u64,
    started_at: String,
}

// Per-model accumulator: (sessions, messages, tokens, cost, duration_ms,
// latest_started_at). The last start is a timestamp, not a pre-formatted
// string, so "last active" sorts chronologically across year boundaries
// (a `%m/%d %H:%M` string compared lexicographically put 12/30 after 01/15).
type ModelAccum = (usize, usize, u64, f64, u64, Option<DateTime<Utc>>);

fn compute_dashboard(projects: &[Project]) -> DashboardAgg {
    let mut total_sessions = 0usize;
    let mut total_msgs = 0usize;
    let mut total_cost = 0.0f64;
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cache_w = 0u64;
    let mut total_cache_r = 0u64;
    let mut by_project: Vec<ProjectAgg> = Vec::with_capacity(projects.len());
    let mut by_model_map: FxHashMap<&str, ModelAccum> = FxHashMap::default();
    let mut top_sessions: Vec<TopSession> = Vec::new();

    for (pi, p) in projects.iter().enumerate() {
        let mut pc = 0.0f64;
        let mut pm = 0usize;
        let mut ptok = 0u64;
        let mut pdur = 0u64;
        let mut plast: Option<DateTime<Utc>> = None;
        for (si, s) in p.sessions.iter().enumerate() {
            let cost = s.cost;
            let sdur = match (s.started_at, s.ended_at) {
                (Some(a), Some(b)) if b > a => (b - a).num_milliseconds().max(0) as u64,
                _ => 0,
            };
            let stok = s.total_input_tokens
                + s.total_output_tokens
                + s.total_cache_read
                + s.total_cache_create;
            let started = s.started_at.map(format_datetime_short).unwrap_or_default();
            total_sessions += 1;
            total_msgs += s.msg_count as usize;
            total_cost += cost;
            total_input += s.total_input_tokens;
            total_output += s.total_output_tokens;
            total_cache_w += s.total_cache_create;
            total_cache_r += s.total_cache_read;
            pc += cost;
            pm += s.msg_count as usize;
            ptok += stok;
            pdur += sdur;
            // Compare chronologically, keep the most recent start.
            if let Some(ts) = s.started_at {
                if plast.is_none_or(|cur| ts > cur) {
                    plast = Some(ts);
                }
            }

            // Borrow-keyed HashMap: model names are a tiny closed set
            // (~5 unique), no String allocation per session.
            let model = s.model.as_deref().unwrap_or("unknown");
            let entry = by_model_map
                .entry(model)
                .or_insert_with(|| (0, 0, 0, 0.0, 0, None));
            entry.0 += 1;
            entry.1 += s.msg_count as usize;
            entry.2 += stok;
            entry.3 += cost;
            entry.4 += sdur;
            if let Some(ts) = s.started_at {
                if entry.5.is_none_or(|cur| ts > cur) {
                    entry.5 = Some(ts);
                }
            }

            top_sessions.push(TopSession {
                pi,
                si,
                cost,
                msgs: s.msg_count as usize,
                tokens: stok,
                duration_ms: sdur,
                started_at: started,
            });
        }
        by_project.push(ProjectAgg {
            idx: pi,
            sess_count: p.sessions.len(),
            msg_count: pm,
            tokens: ptok,
            cost: pc,
            duration_ms: pdur,
            last_active: plast.map(format_datetime_short).unwrap_or_default(),
        });
    }

    by_project.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    top_sessions.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    top_sessions.truncate(50);

    let mut by_model: Vec<ModelAgg> = by_model_map
        .into_iter()
        .map(|(name, (sess, msgs, tok, cost, dur, last))| ModelAgg {
            name: name.to_string(),
            sess_count: sess,
            msg_count: msgs,
            tokens: tok,
            cost,
            duration_ms: dur,
            last_active: last.map(format_datetime_short).unwrap_or_default(),
        })
        .collect();
    by_model.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    DashboardAgg {
        total_sessions,
        total_msgs,
        total_cost,
        total_input,
        total_output,
        total_cache_w,
        total_cache_r,
        by_project,
        by_model,
        top_sessions,
    }
}

impl App {
    pub fn new(
        projects: Vec<Project>,
        source: &'static dyn crate::source::Source,
        key_overrides: &std::collections::BTreeMap<String, String>,
    ) -> Self {
        let (keys, warnings) = crate::keymap::Keymap::new(key_overrides);
        let count = projects.len();
        let filtered: Vec<usize> = (0..count).collect();
        let mut state = ListState::default();
        if count > 0 {
            state.select(Some(0));
        }
        let dashboard = compute_dashboard(&projects);
        Self {
            projects,
            source,
            searcher: Searcher::new(),
            view: View::Projects,
            search_query: String::new(),
            searching: false,
            project_state: state,
            session_state: ListState::default(),
            message_scroll: 0,
            message_max_scroll: 0,
            message_lines: Vec::new(),
            message_lines_key: None,
            pending_load: false,
            selected_project: None,
            selected_session: None,
            filtered_projects: filtered,
            filtered_sessions: vec![],
            dash_scroll: 0,
            dash_max_scroll: 0,
            session_detail_open: false,
            keys_open: false,
            keys,
            palette_open: false,
            palette_query: String::new(),
            palette_sel: 0,
            warnings,
            post_action: None,
            quit: false,
            dashboard,
        }
    }

    pub fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> std::io::Result<()> {
        // Draw once up front, then redraw only after an event was
        // actually received and handled. Nothing in this UI is
        // time-based, so blocking on input makes idle cost zero — a
        // timed loop repaints the whole session ~20×/s for nothing.
        let _ = terminal.draw(|f| self.render(f))?;
        while !self.quit {
            if event::poll(Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(key) => {
                        // Windows reports both Press and Release; act on
                        // Press only so each keystroke fires once.
                        if key.kind == KeyEventKind::Press {
                            self.handle_key(key);
                        }
                    }
                    Event::Paste(text) => self.handle_paste(&text),
                    // Resize (and everything else) falls through to the
                    // redraw below — ratatui only autoresizes inside
                    // `draw`, so skipping it left a stale clipped frame
                    // until the next keypress.
                    _ => {}
                }
                let _ = terminal.draw(|f| self.render(f))?;
                // Deferred session load: the frame above showed the
                // "loading" placeholder; now do the blocking work and
                // repaint with the content.
                if self.pending_load {
                    self.load_selected_session();
                    let _ = terminal.draw(|f| self.render(f))?;
                }
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: event::KeyEvent) {
        if self.searching {
            // Ctrl+C quits even while typing a search query, rather than
            // inserting a literal 'c'.
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.quit = true;
                return;
            }
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.search_query.clear();
                    self.update_filter();
                }
                KeyCode::Enter => {
                    self.searching = false;
                }
                KeyCode::Backspace => {
                    let _ = self.search_query.pop();
                    self.update_filter();
                }
                // Control chords (Ctrl+U, ...) still arrive as Char;
                // don't insert the literal letter into the query.
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.search_query.push(c);
                    self.update_filter();
                }
                _ => {}
            }
            return;
        }

        // The key overlay swallows the next keystroke: any key closes it,
        // rather than firing an action against a view the overlay is
        // covering.
        if self.keys_open {
            self.keys_open = false;
            return;
        }

        if self.palette_open {
            self.handle_palette_key(key);
            return;
        }

        // Not rebindable: a config that lost it would be unquittable.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        let action = to_key(key).and_then(|k| self.keys.action(k));
        // Arrows/Enter/Esc keep their meaning whatever `[keys]` says.
        let action = action.or(match key.code {
            KeyCode::Down => Some(Act::Down),
            KeyCode::Up => Some(Act::Up),
            KeyCode::Right | KeyCode::Enter => Some(Act::Open),
            KeyCode::Left => Some(Act::Back),
            KeyCode::Esc => Some(Act::Quit),
            _ => None,
        });

        // Paging is keyed off the physical keys only; nothing binds it.
        match key.code {
            KeyCode::PageDown => {
                self.page(20);
                return;
            }
            KeyCode::PageUp => {
                self.page(-20);
                return;
            }
            _ => {}
        }

        let Some(action) = action else { return };
        self.run_action(action);
    }

    fn run_action(&mut self, action: Act) {
        match action {
            Act::Help => self.keys_open = true,
            Act::Palette => {
                self.palette_open = true;
                self.palette_query.clear();
                self.palette_sel = 0;
            }
            // A committed filter (typed, then Enter) hides the input box
            // but keeps the list narrowed — clear it before quitting or
            // stepping back.
            Act::Quit if !self.search_query.is_empty() => {
                self.search_query.clear();
                self.update_filter();
            }
            Act::Quit => match self.view {
                View::Projects => self.quit = true,
                View::Dashboard => self.view = View::Projects,
                // Same semantics as Back — keep the message-eviction
                // logic in back() the single exit path from a session.
                View::Sessions | View::Messages => self.back(),
            },
            // Handed to main.rs after teardown so `claude` gets a
            // clean terminal.
            Act::Resume => {
                if let Some((id, cwd)) = self.current_resume_target() {
                    self.post_action = Some(PostAction::Resume { id, cwd });
                    self.quit = true;
                }
            }
            Act::Web => {
                self.post_action = Some(PostAction::OpenWeb);
                self.quit = true;
            }
            Act::Detail => {
                if self.view == View::Sessions {
                    self.session_detail_open = !self.session_detail_open;
                }
            }
            Act::Dashboard => {
                if self.view == View::Dashboard {
                    self.view = View::Projects;
                } else {
                    self.view = View::Dashboard;
                    self.dash_scroll = 0;
                }
            }
            // Only the list-layout views (Projects, Sessions) render a
            // search input box; entering search mode elsewhere would
            // silently swallow keystrokes with no visible field.
            Act::Search => {
                if matches!(self.view, View::Projects | View::Sessions) {
                    self.searching = true;
                    self.search_query.clear();
                    // Without this the previous query's filtered list stays
                    // on screen under an empty box until the next keystroke.
                    self.update_filter();
                }
            }
            Act::Down => self.move_down(),
            Act::Up => self.move_up(),
            Act::Top => self.move_top(),
            Act::Bottom => self.move_bottom(),
            Act::Open => self.enter(),
            Act::Back => self.back(),
        }
    }

    fn page(&mut self, delta: i32) {
        let step = delta.unsigned_abs() as u16;
        let (scroll, max) = match self.view {
            View::Messages => (&mut self.message_scroll, self.message_max_scroll),
            View::Dashboard => (&mut self.dash_scroll, self.dash_max_scroll),
            _ => return,
        };
        *scroll = if delta > 0 {
            scroll.saturating_add(step).min(max)
        } else {
            scroll.saturating_sub(step)
        };
    }

    /// Palette rows matching the current query, as (key, label, action).
    fn palette_rows(&self) -> Vec<(String, &'static str, Act)> {
        let q = self.palette_query.to_lowercase();
        self.keys
            .palette_entries()
            .into_iter()
            .filter(|(_, what, _)| q.is_empty() || what.to_lowercase().contains(&q))
            .map(|(k, what, a)| (k.to_string(), what, a))
            .collect()
    }

    fn handle_palette_key(&mut self, key: event::KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.palette_open = false;
            }
            KeyCode::Enter => {
                let picked = self
                    .palette_rows()
                    .get(self.palette_sel)
                    .map(|(_, _, a)| *a);
                self.palette_open = false;
                if let Some(a) = picked {
                    self.run_action(a);
                }
            }
            KeyCode::Down => self.palette_sel = self.palette_sel.saturating_add(1),
            KeyCode::Up => self.palette_sel = self.palette_sel.saturating_sub(1),
            KeyCode::Backspace => {
                let _ = self.palette_query.pop();
                self.palette_sel = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.palette_query.push(c);
                self.palette_sel = 0;
            }
            _ => {}
        }
        let n = self.palette_rows().len();
        self.palette_sel = self.palette_sel.min(n.saturating_sub(1));
    }

    // Bracketed paste: text goes into the search query when one is being
    // typed; anywhere else it is dropped so pasted characters are never
    // interpreted as commands ('q' quit, 'c' resume, ...).
    fn handle_paste(&mut self, text: &str) {
        if self.searching {
            self.search_query.push_str(text);
            self.update_filter();
        }
    }

    fn move_down(&mut self) {
        match self.view {
            View::Projects => {
                let len = self.filtered_projects.len();
                if len == 0 {
                    return;
                }
                let i = self
                    .project_state
                    .selected()
                    .map_or(0, |i| if i + 1 >= len { i } else { i + 1 });
                self.project_state.select(Some(i));
            }
            View::Sessions => {
                let len = self.filtered_sessions.len();
                if len == 0 {
                    return;
                }
                let i = self
                    .session_state
                    .selected()
                    .map_or(0, |i| if i + 1 >= len { i } else { i + 1 });
                self.session_state.select(Some(i));
            }
            View::Messages => {
                self.message_scroll = self
                    .message_scroll
                    .saturating_add(3)
                    .min(self.message_max_scroll);
            }
            View::Dashboard => {
                self.dash_scroll = self.dash_scroll.saturating_add(3).min(self.dash_max_scroll);
            }
        }
    }

    fn move_up(&mut self) {
        match self.view {
            View::Projects => {
                let i = self
                    .project_state
                    .selected()
                    .map_or(0, |i| i.saturating_sub(1));
                self.project_state.select(Some(i));
            }
            View::Sessions => {
                let i = self
                    .session_state
                    .selected()
                    .map_or(0, |i| i.saturating_sub(1));
                self.session_state.select(Some(i));
            }
            View::Messages => {
                self.message_scroll = self.message_scroll.saturating_sub(3);
            }
            View::Dashboard => {
                self.dash_scroll = self.dash_scroll.saturating_sub(3);
            }
        }
    }

    const fn move_top(&mut self) {
        match self.view {
            View::Projects => self.project_state.select(Some(0)),
            View::Sessions => self.session_state.select(Some(0)),
            View::Messages => self.message_scroll = 0,
            View::Dashboard => self.dash_scroll = 0,
        }
    }

    const fn move_bottom(&mut self) {
        match self.view {
            View::Projects => {
                let len = self.filtered_projects.len();
                if len > 0 {
                    self.project_state.select(Some(len - 1));
                }
            }
            View::Sessions => {
                let len = self.filtered_sessions.len();
                if len > 0 {
                    self.session_state.select(Some(len - 1));
                }
            }
            View::Messages => {
                self.message_scroll = self.message_max_scroll;
            }
            View::Dashboard => {
                self.dash_scroll = self.dash_max_scroll;
            }
        }
    }

    fn enter(&mut self) {
        match self.view {
            View::Projects => self.enter_project(),
            View::Sessions => self.enter_session(),
            View::Messages | View::Dashboard => {}
        }
    }

    fn enter_project(&mut self) {
        let Some(i) = self.project_state.selected() else {
            return;
        };
        let Some(&pi) = self.filtered_projects.get(i) else {
            return;
        };
        self.selected_project = Some(pi);
        self.view = View::Sessions;
        self.search_query.clear();
        self.update_filter();
        self.session_state.select(Some(0));
    }

    fn enter_session(&mut self) {
        let Some(i) = self.session_state.selected() else {
            return;
        };
        let Some(&si) = self.filtered_sessions.get(i) else {
            return;
        };
        let Some(pi) = self.selected_project else {
            return;
        };
        // Lazy-load messages — only the session the user opened pays
        // the deserialize cost. The load itself is deferred to `run`
        // (see `pending_load`) so a frame with the loading placeholder
        // reaches the terminal before any blocking parse.
        self.pending_load = self
            .projects
            .get(pi)
            .and_then(|p| p.sessions.get(si))
            .is_some_and(|s| s.messages.is_empty());
        self.selected_session = Some(si);
        self.view = View::Messages;
        self.message_scroll = 0;
    }

    fn load_selected_session(&mut self) {
        self.pending_load = false;
        let (Some(pi), Some(si)) = (self.selected_project, self.selected_session) else {
            return;
        };
        let source = self.source;
        if let Some(s) = self
            .projects
            .get_mut(pi)
            .and_then(|p| p.sessions.get_mut(si))
        {
            if s.messages.is_empty() {
                let path = s.file_path.clone();
                let _ = parse::ensure_messages_loaded(source, s, &path);
            }
        }
    }

    fn back(&mut self) {
        match self.view {
            View::Projects => {}
            View::Dashboard => self.view = View::Projects,
            View::Sessions => {
                self.view = View::Projects;
                self.selected_project = None;
                self.search_query.clear();
                self.update_filter();
            }
            View::Messages => {
                self.view = View::Sessions;
                // Evict the message blob and its rendered-line cache —
                // both reload cheaply from the .msgs cache on re-entry,
                // and keeping every visited session's messages resident
                // grew without bound over a long browse.
                if let Some(si) = self.selected_session {
                    if let Some(s) = self
                        .selected_project
                        .and_then(|pi| self.projects.get_mut(pi))
                        .and_then(|p| p.sessions.get_mut(si))
                    {
                        s.messages = Vec::new();
                    }
                }
                self.message_lines = Vec::new();
                self.message_lines_key = None;
                self.selected_session = None;
                self.message_scroll = 0;
            }
        }
    }

    /// `(conversation_id, cwd)` for the highlighted session. The id is the
    /// resumable conversation, which for a subagent transcript is its
    /// parent — see `claude_code::resume_target_of`.
    fn current_resume_target(&self) -> Option<(String, Option<String>)> {
        let session = match self.view {
            View::Sessions => {
                let pi = self.selected_project?;
                let i = self.session_state.selected()?;
                let &si = self.filtered_sessions.get(i)?;
                self.projects.get(pi)?.sessions.get(si)?
            }
            View::Messages => {
                let pi = self.selected_project?;
                let si = self.selected_session?;
                self.projects.get(pi)?.sessions.get(si)?
            }
            _ => return None,
        };
        let id = crate::source::claude_code::resume_target_of(&session.file_path)
            .unwrap_or_else(|| session.id.clone());
        Some((id, session.cwd.clone()))
    }

    fn update_filter(&mut self) {
        // Disjoint-field borrows (edition 2024): we read
        // self.search_query / self.searcher / self.projects while
        // writing self.filtered_projects / self.filtered_sessions —
        // no query clone per keystroke.
        match self.view {
            View::Projects => {
                if self.search_query.is_empty() {
                    self.filtered_projects = (0..self.projects.len()).collect();
                } else {
                    self.filtered_projects = (0..self.projects.len())
                        .filter(|&i| {
                            self.searcher
                                .matches(&self.search_query, &self.projects[i].name)
                        })
                        .collect();
                }
                if self.filtered_projects.is_empty() {
                    self.project_state.select(None);
                } else {
                    self.project_state.select(Some(0));
                }
            }
            View::Sessions => {
                if let Some(pi) = self.selected_project {
                    let sessions = &self.projects[pi].sessions;
                    if self.search_query.is_empty() {
                        self.filtered_sessions = (0..sessions.len()).collect();
                    } else {
                        self.filtered_sessions = (0..sessions.len())
                            .filter(|&i| {
                                self.searcher
                                    .matches(&self.search_query, sessions[i].display_name())
                            })
                            .collect();
                    }
                    if self.filtered_sessions.is_empty() {
                        self.session_state.select(None);
                    } else {
                        self.session_state.select(Some(0));
                    }
                }
            }
            View::Messages | View::Dashboard => {}
        }
    }

    fn render(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(f.area());

        match self.view {
            View::Projects => self.render_projects(f, chunks[0]),
            View::Sessions => self.render_sessions(f, chunks[0]),
            View::Messages => self.render_messages(f, chunks[0]),
            View::Dashboard => self.render_dashboard(f, chunks[0]),
        }

        self.render_statusbar(f, chunks[1]);
        if self.keys_open {
            render_key_help(f, &self.keys);
        }
        if self.palette_open {
            render_palette(
                f,
                &self.palette_query,
                self.palette_sel,
                &self.palette_rows(),
            );
        }
    }

    /// Status-bar segment for an active filter, e.g. ` | filter "auth" 3/91`.
    /// Enter leaves search mode but keeps the query, so without this the
    /// only evidence of the filter is missing rows.
    fn filter_note(&self) -> String {
        if self.search_query.is_empty() {
            return String::new();
        }
        let (shown, total) = match self.view {
            View::Projects => (self.filtered_projects.len(), self.projects.len()),
            View::Sessions => (
                self.filtered_sessions.len(),
                self.selected_project
                    .and_then(|pi| self.projects.get(pi))
                    .map_or(0, |p| p.sessions.len()),
            ),
            View::Messages | View::Dashboard => return String::new(),
        };
        format!(" | filter {:?} {shown}/{total}", self.search_query)
    }

    /// Per-view hint strip, built from the live keymap so a rebind shows
    /// the key the user actually has.
    fn render_statusbar(&self, f: &mut Frame, area: Rect) {
        // Status bar repaints on every key. The dashboard struct holds the
        // global totals (computed once in App::new) so we don't re-walk
        // every session per frame.
        // Hints come from the live keymap, so a rebind shows the real key.
        let k = |a: Act| self.keys.key_for(a);
        let (left, right) = match self.view {
            View::Projects => (
                format!(
                    " {} projects | {} sessions | {}{}",
                    self.projects.len(),
                    format_number(self.dashboard.total_sessions as u64),
                    format_cost(self.dashboard.total_cost),
                    self.filter_note(),
                ),
                format!(
                    "{}/{} nav | {}/{} ends | {} search | enter select | {} web | {} dash | {} keys | {} quit ",
                    k(Act::Down),
                    k(Act::Up),
                    k(Act::Top),
                    k(Act::Bottom),
                    k(Act::Search),
                    k(Act::Web),
                    k(Act::Dashboard),
                    k(Act::Help),
                    k(Act::Quit),
                ),
            ),
            View::Sessions => {
                if let Some(pi) = self.selected_project {
                    let p = &self.projects[pi];
                    (
                        format!(
                            " {} | {} sessions | {}{}",
                            p.name,
                            format_number(p.sessions.len() as u64),
                            format_cost(p.total_cost),
                            self.filter_note(),
                        ),
                        format!(
                            "{}/{} nav | {}/{} ends | {} search | enter view | {} detail | {} resume | {} keys | esc back ",
                            k(Act::Down),
                            k(Act::Up),
                            k(Act::Top),
                            k(Act::Bottom),
                            k(Act::Search),
                            k(Act::Detail),
                            k(Act::Resume),
                            k(Act::Help),
                        ),
                    )
                } else {
                    (String::new(), String::new())
                }
            }
            View::Messages => {
                if let (Some(pi), Some(si)) = (self.selected_project, self.selected_session) {
                    let s = &self.projects[pi].sessions[si];
                    (
                        format!(
                            " {} msgs | {} | {}",
                            format_number(u64::from(s.msg_count)),
                            s.model.as_deref().unwrap_or("?"),
                            format_cost(s.cost),
                        ),
                        format!(
                            "{}/{} scroll | {}/{} ends | PgDn/PgUp page | {} resume | {} keys | esc back ",
                            k(Act::Down),
                            k(Act::Up),
                            k(Act::Top),
                            k(Act::Bottom),
                            k(Act::Resume),
                            k(Act::Help),
                        ),
                    )
                } else {
                    (String::new(), String::new())
                }
            }
            View::Dashboard => (
                format!(
                    " dashboard | {} total",
                    format_cost(self.dashboard.total_cost)
                ),
                format!(
                    "{}/{} scroll | {}/{} ends | {} close | {} run | {} keys | {} quit ",
                    k(Act::Down),
                    k(Act::Up),
                    k(Act::Top),
                    k(Act::Bottom),
                    k(Act::Dashboard),
                    k(Act::Palette),
                    k(Act::Help),
                    k(Act::Quit),
                ),
            ),
        };

        // Otherwise invisible: the key just does nothing, and the
        // alternate screen has swallowed stderr.
        let left = match self.warnings.first() {
            Some(w) => format!("{left} | config: {w}"),
            None => left,
        };

        // Display width, not byte len — project names can be non-ASCII
        // and byte counting left the right-hand hints short of the edge.
        let padding_len = (area.width as usize)
            .saturating_sub(unicode_width::UnicodeWidthStr::width(left.as_str()))
            .saturating_sub(unicode_width::UnicodeWidthStr::width(right.as_str()));
        let bg = Style::default().bg(style::tui(style::BG2));
        let full_bar = Line::from(vec![
            Span::styled(left, bg.fg(style::tui(style::FG))),
            Span::styled(" ".repeat(padding_len), bg),
            Span::styled(right, bg.fg(style::tui(style::FG2))),
        ]);
        f.render_widget(Paragraph::new(full_bar), area);
    }

    fn render_projects(&mut self, f: &mut Frame, area: Rect) {
        let chunks = list_layout(area, self.searching, false);
        if self.searching {
            self.render_search_input(f, chunks[0]);
        }
        f.render_widget(unified_header_line(LIST_COLS_PROJECTS), chunks[1]);

        let items: Vec<ListItem> = self
            .filtered_projects
            .iter()
            .map(|&i| {
                let p = &self.projects[i];
                let date = p
                    .last_active
                    .map(format_datetime_short)
                    .unwrap_or_else(|| "?".into());
                // All four numeric columns are precomputed on `Project`
                // — no per-frame summing across sessions.
                ListItem::new(unified_row_line(
                    &date,
                    &p.name,
                    Some(p.sessions.len() as u64),
                    p.total_msgs,
                    p.total_tokens,
                    p.total_cost,
                    p.total_dur_ms,
                ))
            })
            .collect();
        f.render_stateful_widget(highlighted_list(items), chunks[2], &mut self.project_state);
    }

    fn render_sessions(&mut self, f: &mut Frame, area: Rect) {
        let Some(pi) = self.selected_project else {
            return;
        };
        let chunks = list_layout(area, self.searching, self.session_detail_open);
        if self.searching {
            self.render_search_input(f, chunks[0]);
        }
        f.render_widget(unified_header_line(LIST_COLS_SESSIONS), chunks[1]);

        let sessions = &self.projects[pi].sessions;
        let items: Vec<ListItem> = self
            .filtered_sessions
            .iter()
            .map(|&i| {
                let s = &sessions[i];
                let date = s
                    .started_at
                    .map(format_datetime_short)
                    .unwrap_or_else(|| "?".into());
                let dur_ms = match (s.started_at, s.ended_at) {
                    (Some(a), Some(b)) if b > a => (b - a).num_milliseconds().max(0) as u64,
                    _ => 0,
                };
                ListItem::new(unified_row_line(
                    &date,
                    s.display_name(),
                    None, // per-session row: session count is degenerate
                    u64::from(s.msg_count),
                    s.total_tokens(),
                    s.cost,
                    dur_ms,
                ))
            })
            .collect();
        f.render_stateful_widget(highlighted_list(items), chunks[2], &mut self.session_state);

        if self.session_detail_open {
            // Detail pane lives in the trailing chunk produced by
            // `list_layout` when `with_detail` is true.
            self.render_session_detail(f, chunks[3], pi);
        }
    }

    fn render_search_input(&self, f: &mut Frame, area: Rect) {
        let input = Paragraph::new(format!(" {}", self.search_query))
            .block(Block::default().borders(Borders::ALL).title(" search "));
        f.render_widget(input, area);
    }

    fn render_session_detail(&self, f: &mut Frame, area: Rect, pi: usize) {
        let Some(idx) = self.session_state.selected() else {
            return;
        };
        let Some(&si) = self.filtered_sessions.get(idx) else {
            return;
        };
        let s = &self.projects[pi].sessions[si];

        let dim = Style::default().fg(style::tui(style::FG3));
        let white = Style::default().fg(style::tui(style::FG));
        let green = Style::default().fg(style::tui(style::GREEN));
        let started = s
            .started_at
            .map(format_datetime)
            .unwrap_or_else(|| "?".into());
        let ended = s
            .ended_at
            .map(format_datetime)
            .unwrap_or_else(|| "?".into());
        let model = s.model.as_deref().unwrap_or("?");
        let first = s
            .first_user_msg
            .as_deref()
            .unwrap_or_else(|| s.display_name());

        let lines = vec![
            Line::from(Span::styled(
                format!(" session {}", s.id),
                Style::default().fg(style::tui(style::DASH_HEADER)),
            )),
            Line::from(vec![
                Span::styled("  started ", dim),
                Span::styled(format!("{started:<18}"), white),
                Span::styled("ended ", dim),
                Span::styled(format!("{ended:<18}"), white),
                Span::styled("model ", dim),
                Span::styled(model.to_string(), white),
            ]),
            Line::from(vec![
                Span::styled("  in ", dim),
                Span::styled(
                    format!("{:<10}", format_number(s.total_input_tokens)),
                    white,
                ),
                Span::styled("out ", dim),
                Span::styled(
                    format!("{:<10}", format_number(s.total_output_tokens)),
                    white,
                ),
                Span::styled("cache-w ", dim),
                Span::styled(
                    format!("{:<10}", format_number(s.total_cache_create)),
                    white,
                ),
                Span::styled("cache-r ", dim),
                Span::styled(format!("{:<10}", format_number(s.total_cache_read)), white),
                Span::styled("cost ", dim),
                Span::styled(format_cost(s.cost), green),
            ]),
            Line::from(vec![
                Span::styled("  first ", dim),
                Span::styled(
                    truncate_line(first, area.width.saturating_sub(10) as usize),
                    white,
                ),
            ]),
        ];

        let para = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(dim)
                .title(" details (tab to close) ")
                .title_style(dim),
        );
        f.render_widget(para, area);
    }

    fn render_messages(&mut self, f: &mut Frame, area: Rect) {
        let (Some(pi), Some(si)) = (self.selected_project, self.selected_session) else {
            return;
        };

        // Messages not in memory yet: show the placeholder and bail
        // before the line cache is built — caching the empty message
        // list under this (session, width) key would serve a blank
        // view after the load completes.
        if self.pending_load {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                " loading session…",
                Style::default().fg(style::tui(style::FG3)),
            )));
            f.render_widget(placeholder, area);
            return;
        }

        // Message content is immutable once loaded, so the full line
        // buffer is built once per (session, width) instead of on every
        // keystroke; per frame we clone only the visible window below.
        let key = (pi, si, area.width);
        if self.message_lines_key != Some(key) {
            self.message_lines = build_message_lines(&self.projects[pi].sessions[si], area.width);
            self.message_lines_key = Some(key);
        }

        // The Block's top title consumes one inner row even with
        // Borders::NONE, so the text viewport is one line shorter than
        // `area` — computing max scroll from area.height made the last
        // line unreachable.
        let visible = area.height.saturating_sub(1);
        // Paragraph/scroll offsets are u16: sessions rendering more than
        // 65535 lines saturate here and the tail is unreachable — a
        // ratatui API limit we accept.
        let total_lines = u16::try_from(self.message_lines.len()).unwrap_or(u16::MAX);
        self.message_max_scroll = total_lines.saturating_sub(visible);
        // Re-clamp: growing the terminal shrinks the max and would
        // otherwise strand the offset past the end of the content.
        self.message_scroll = self.message_scroll.min(self.message_max_scroll);

        let title = self.projects[pi].sessions[si].display_name();
        // Boundary-safe truncation — `title` is arbitrary user text, so
        // slicing raw bytes (`&title[..57]`) panicked when byte 57 fell
        // mid-codepoint. `truncate_line` truncates on a char boundary and
        // appends "..." itself when over the cap.
        let truncated_title = format!(" {} ", truncate_line(title, 60));

        let start = self.message_scroll as usize;
        let end = (start + visible as usize).min(self.message_lines.len());
        let window: Vec<Line<'static>> = self.message_lines[start..end].to_vec();

        let paragraph = Paragraph::new(window).block(
            Block::default()
                .borders(Borders::NONE)
                .title(truncated_title)
                .title_style(Style::default().fg(style::tui(style::FG3))),
        );

        f.render_widget(paragraph, area);

        // Scrollbar
        if self.message_max_scroll > 0 {
            let mut scrollbar_state = ScrollbarState::new(self.message_max_scroll as usize)
                .position(self.message_scroll as usize);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut scrollbar_state,
            );
        }
    }

    fn render_dashboard(&mut self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        let w = area.width as usize;

        // Read the cached aggregation. Computed once in App::new().
        let d = &self.dashboard;
        let (
            total_sessions,
            total_msgs,
            total_cost,
            total_input,
            total_output,
            total_cache_w,
            total_cache_r,
        ) = (
            d.total_sessions,
            d.total_msgs,
            d.total_cost,
            d.total_input,
            d.total_output,
            d.total_cache_w,
            d.total_cache_r,
        );

        let dim = Style::default().fg(style::tui(style::FG3));
        let head = Style::default().fg(style::tui(style::DASH_HEADER));
        let val = Style::default()
            .fg(style::tui(style::FG))
            .add_modifier(Modifier::BOLD);
        let green = Style::default()
            .fg(style::tui(style::GREEN))
            .add_modifier(Modifier::BOLD);

        // Header
        lines.push(Line::from(Span::styled(" OVERVIEW", head)));
        lines.push(Line::from(""));

        // Overview grid: 4 cells per row, each cell is a fixed-width
        // label (10) + fixed-width value (16). Both rows use the same
        // cell widths so label and value columns line up vertically,
        // regardless of how short or long any individual value is.
        const LBL_W: usize = 10;
        const VAL_W: usize = 16;
        let cell = |label: &str, value: String, val_style: Style| -> Vec<Span<'static>> {
            vec![
                Span::styled(format!("  {:<LBL_W$}", label.to_string()), dim),
                Span::styled(format!("{value:<VAL_W$}"), val_style),
            ]
        };

        // Stats row 1
        let mut row1: Vec<Span> = Vec::new();
        row1.extend(cell("Sessions", format_number(total_sessions as u64), val));
        row1.extend(cell("Messages", format_number(total_msgs as u64), val));
        row1.extend(cell("Projects", format!("{}", self.projects.len()), val));
        row1.extend(cell("Cost", format_cost(total_cost), green));
        lines.push(Line::from(row1));
        lines.push(Line::from(""));

        // Stats row 2 — same cell grid as row 1 so columns line up.
        let mut row2: Vec<Span> = Vec::new();
        // `format_number` (underscored thousands, same as web/CLI) —
        // a K/M abbreviation drops the thousands separator inside the
        // mantissa and reads as a bare "12884.5M".
        row2.extend(cell("Input", format_number(total_input), val));
        row2.extend(cell("Output", format_number(total_output), val));
        row2.extend(cell("Cache-W", format_number(total_cache_w), val));
        row2.extend(cell("Cache-R", format_number(total_cache_r), val));
        lines.push(Line::from(row2));
        lines.push(Line::from(""));

        // All three breakdown sections use the unified column schema
        // (date | name | sessions | messages | tokens | cost | duration)
        // so the TUI mirrors the web's dashboard layout column-for-column.
        // No collapse/expand — the TUI shows the whole list.

        // By project
        lines.push(Line::from(Span::styled("─".repeat(w), dim)));
        lines.push(Line::from(Span::styled(" BY PROJECT", head)));
        lines.push(Line::from(""));
        lines.push(unified_header_line_line(LIST_COLS_PROJECTS));
        for pa in &d.by_project {
            let name = self.projects[pa.idx].name.as_str();
            lines.push(unified_row_line(
                &pa.last_active,
                name,
                Some(pa.sess_count as u64),
                pa.msg_count as u64,
                pa.tokens,
                pa.cost,
                pa.duration_ms,
            ));
        }
        lines.push(Line::from(""));

        // By model
        lines.push(Line::from(Span::styled("─".repeat(w), dim)));
        lines.push(Line::from(Span::styled(" BY MODEL", head)));
        lines.push(Line::from(""));
        lines.push(unified_header_line_line(&[
            UCol {
                label: "date",
                width: 12,
            },
            UCol {
                label: "model",
                width: 0,
            },
            UCol {
                label: "sessions",
                width: 9,
            },
            UCol {
                label: "messages",
                width: 9,
            },
            UCol {
                label: "tokens",
                width: 13,
            },
            UCol {
                label: "cost",
                width: 12,
            },
            UCol {
                label: "duration",
                width: 10,
            },
        ]));
        for ma in &d.by_model {
            lines.push(unified_row_line(
                &ma.last_active,
                &ma.name,
                Some(ma.sess_count as u64),
                ma.msg_count as u64,
                ma.tokens,
                ma.cost,
                ma.duration_ms,
            ));
        }
        lines.push(Line::from(""));

        // By session (top by cost)
        lines.push(Line::from(Span::styled("─".repeat(w), dim)));
        lines.push(Line::from(Span::styled(" BY SESSION (top by cost)", head)));
        lines.push(Line::from(""));
        lines.push(unified_header_line_line(LIST_COLS_SESSIONS));
        for ts in d.top_sessions.iter().take(20) {
            let project = self.projects[ts.pi].name.as_str();
            let name = self.projects[ts.pi].sessions[ts.si].display_name();
            let ident = format!("{project} / {name}");
            lines.push(unified_row_line(
                &ts.started_at,
                &ident,
                None, // per-session row: sessions column degenerate
                ts.msgs as u64,
                ts.tokens,
                ts.cost,
                ts.duration_ms,
            ));
        }

        let total_lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.dash_max_scroll = total_lines.saturating_sub(area.height);
        // Re-clamp so a resize can't strand the offset past the content.
        self.dash_scroll = self.dash_scroll.min(self.dash_max_scroll);

        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::NONE))
            .scroll((self.dash_scroll, 0));

        f.render_widget(paragraph, area);

        if self.dash_max_scroll > 0 {
            let mut scrollbar_state = ScrollbarState::new(self.dash_max_scroll as usize)
                .position(self.dash_scroll as usize);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut scrollbar_state,
            );
        }
    }
}

// Build the full rendered line buffer for one session at `width`
// columns. Called from render_messages only when its (session, width)
// cache key changes — everything here is build-time cost, not per-frame.
fn build_message_lines(session: &parse::Session, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let aw = width as usize;

    // Identical for every user message at this width; cloned per use so
    // the cached lines own their text.
    let separator: String = "─".repeat(aw);
    let separator_style = Style::default().fg(style::tui(style::SEPARATOR));

    for msg in &session.messages {
        // Tag colors read from the shared `style` tokens — same
        // palette the web `.tg-*` CSS uses, so colors match across
        // TUI and web for each message kind.
        let (tag, tag_color) = match msg.kind {
            MessageKind::User => ("USR", style::tui(style::K_USER)),
            MessageKind::Assistant => ("AI ", style::tui(style::K_ASSISTANT)),
            MessageKind::ToolUse => (">>>", style::tui(style::K_TOOLUSE)),
            MessageKind::ToolResult => ("<<<", style::tui(style::K_TOOLRESULT)),
            MessageKind::Thinking => ("...", style::tui(style::K_THINKING)),
            MessageKind::System => ("SYS", style::tui(style::K_SYSTEM)),
        };

        if msg.kind == MessageKind::User {
            lines.push(Line::from(Span::styled(separator.clone(), separator_style)));
        }

        // Tag is rendered on the RIGHT of the first line (matches
        // the web's msg-meta convention — "who" on the right, body
        // left). Continuation lines are full-width with no indent
        // since there's no left prefix to align under.
        let tag_text = match &msg.kind {
            MessageKind::ToolUse => {
                let tool = msg.tool_name.as_deref().unwrap_or("tool");
                format!(" {tool} ")
            }
            _ => format!(" {tag} "),
        };
        let tag_w = unicode_width::UnicodeWidthStr::width(tag_text.as_str());
        let body_color = if msg.kind == MessageKind::ToolResult {
            style::tui(style::FG3)
        } else {
            style::tui(style::FG)
        };
        // 1 leading space + content + fill + tag (fixed right-edge)
        let content_w = aw.saturating_sub(tag_w + 2);

        let mut iter = msg.content.lines();
        if let Some(first) = iter.next() {
            let body = truncate_line(first, content_w);
            // Display width, not chars().count() — wide chars (CJK,
            // emoji) otherwise over-pad and push the tag off-screen.
            let pad =
                content_w.saturating_sub(unicode_width::UnicodeWidthStr::width(body.as_str()));
            let spaces: Cow<'static, str> = if pad <= SPACES.len() {
                Cow::Borrowed(&SPACES[..pad])
            } else {
                // Terminals wider than the slab (~64 + tag cols) —
                // allocate instead of clamping the tag mid-line.
                Cow::Owned(" ".repeat(pad))
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {body}"), Style::default().fg(body_color)),
                Span::raw(spaces),
                Span::styled(tag_text, Style::default().fg(tag_color)),
            ]));
        }
        for line in iter.by_ref().take(20) {
            lines.push(Line::from(Span::styled(
                format!(" {}", truncate_line(line, aw.saturating_sub(1))),
                Style::default().fg(body_color),
            )));
        }
        let extra = iter.count();
        if extra > 0 {
            lines.push(Line::from(Span::styled(
                format!(" ... ({extra} more lines)"),
                Style::default().fg(style::tui(style::FG3)),
            )));
        }
    }

    lines
}

// Pad or truncate a string to an exact display width. Truncation uses
// "…" so the final column always occupies the same number of cells.
fn fit_width(s: &str, width: usize) -> String {
    let len = unicode_width::UnicodeWidthStr::width(s);
    match len.cmp(&width) {
        std::cmp::Ordering::Equal => s.to_string(),
        std::cmp::Ordering::Less => format!("{s}{}", " ".repeat(width - len)),
        std::cmp::Ordering::Greater => {
            let mut acc = String::new();
            let mut used = 0usize;
            for c in s.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if used + cw + 1 > width {
                    break;
                }
                acc.push(c);
                used += cw;
            }
            acc.push('…');
            used += 1;
            if used < width {
                acc.push_str(&" ".repeat(width - used));
            }
            acc
        }
    }
}

// ── Unified list columns — mirror the web's `buildTable` schema ──
//
// Column widths are fixed (except `name`, which flexes via fit_width)
// so the right edge of every numeric column aligns across projects and
// sessions views. Any view that renders a list of entities uses these
// helpers; all column decisions live here.

/// Column descriptor: `(label, width_cells)`. Width 0 = flex (only valid
/// for the `name` column). Right-aligned except `name`.
struct UCol {
    label: &'static str,
    width: usize,
}

const LIST_COLS_PROJECTS: &[UCol] = &[
    UCol {
        label: "date",
        width: 12,
    },
    UCol {
        label: "project",
        width: 0,
    },
    UCol {
        label: "sessions",
        width: 9,
    },
    UCol {
        label: "messages",
        width: 9,
    },
    UCol {
        label: "tokens",
        width: 13,
    },
    UCol {
        label: "cost",
        width: 12,
    },
    UCol {
        label: "duration",
        width: 10,
    },
];
const LIST_COLS_SESSIONS: &[UCol] = &[
    UCol {
        label: "date",
        width: 12,
    },
    UCol {
        label: "session",
        width: 0,
    },
    UCol {
        label: "sessions",
        width: 9,
    },
    UCol {
        label: "messages",
        width: 9,
    },
    UCol {
        label: "tokens",
        width: 13,
    },
    UCol {
        label: "cost",
        width: 12,
    },
    UCol {
        label: "duration",
        width: 10,
    },
];

// Shared list scaffolding for Projects + Sessions views. Layout is
// always [search?] [header] [list] [detail?]; chunks unused on a given
// view shrink to length 0. Returning four chunks unconditionally keeps
// the indexing in `render_projects`/`render_sessions` uniform.
fn list_layout(area: Rect, searching: bool, with_detail: bool) -> std::rc::Rc<[Rect]> {
    let search_h = if searching { 3 } else { 0 };
    let detail_h = if with_detail { 7 } else { 0 };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(search_h),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(detail_h),
        ])
        .split(area)
}

fn highlighted_list(items: Vec<ListItem>) -> List {
    List::new(items).highlight_style(
        Style::default()
            .bg(style::tui(style::ROW_SEL_BG))
            .fg(style::tui(style::ACCENT))
            .add_modifier(Modifier::BOLD),
    )
}

fn unified_header_line_line(cols: &[UCol]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols.len());
    for c in cols {
        let w = if c.width == 0 { 24 } else { c.width };
        let s = if c.label == "date"
            || c.label == "project"
            || c.label == "session"
            || c.label == "model"
        {
            format!(" {:<w$}", c.label, w = w)
        } else {
            // Two leading spaces — mirror `unified_row_line`'s numeric
            // column padding so headers sit over their values.
            format!("  {:>w$}", c.label, w = w)
        };
        spans.push(Span::styled(s, Style::default().fg(style::tui(style::FG3))));
    }
    Line::from(spans)
}
fn unified_header_line(cols: &[UCol]) -> Paragraph<'static> {
    Paragraph::new(unified_header_line_line(cols))
}

fn fmt_dur_ms(ms: u64) -> String {
    if ms == 0 {
        return String::new();
    }
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else if s < 86400 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d {}h", s / 86400, (s % 86400) / 3600)
    }
}

/// Build one `Line` for a data row matching `LIST_COLS_*`. `sessions`
/// is `None` when the column is degenerate for the current view
/// (e.g. a sessions list where each row IS one session) — renders as
/// a dim em-dash to preserve the column grid.
fn unified_row_line(
    date: &str,
    name: &str,
    sessions: Option<u64>,
    messages: u64,
    tokens: u64,
    cost: f64,
    duration_ms: u64,
) -> Line<'static> {
    let fg = Style::default().fg(style::tui(style::FG));
    let dim = Style::default().fg(style::tui(style::FG3));
    let sess_s = sessions
        .map(format_number)
        .unwrap_or_else(|| "—".to_string());
    // Two leading spaces on every numeric column so adjacent
    // right-aligned numbers can't be misread as one long number (e.g.
    // "19_012 2_932_455_290" would otherwise blur together).
    Line::from(vec![
        Span::styled(format!(" {date:<12}"), dim),
        Span::styled(format!(" {} ", fit_width(name, 24)), fg),
        Span::styled(format!("  {sess_s:>9}"), dim),
        Span::styled(format!("  {:>9}", format_number(messages)), dim),
        Span::styled(format!("  {:>13}", format_number(tokens)), dim),
        Span::styled(
            format!("  {:>12}", format_cost(cost)),
            Style::default().fg(style::tui(style::GREEN)),
        ),
        Span::styled(format!("  {:>10}", fmt_dur_ms(duration_ms)), dim),
    ])
}

/// Centered `?` overlay, rendered from the live keymap. `Clear` stops
/// the view underneath bleeding through.
fn render_key_help(f: &mut Frame, keys: &crate::keymap::Keymap) {
    let rows = keys.help();
    let area = f.area();
    let w = 54u16.min(area.width.saturating_sub(2));
    let h = (rows.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = centered(area, w, h);
    let group_w = rows.iter().map(|(g, ..)| g.len()).max().unwrap_or(0);
    let key_w = rows
        .iter()
        .map(|(_, k, ..)| unicode_width::UnicodeWidthStr::width(k.as_str()))
        .max()
        .unwrap_or(0);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .map(|(group, keys, what, _)| {
            let pad_g = " ".repeat(group_w.saturating_sub(group.len()));
            let pad_k = " "
                .repeat(key_w.saturating_sub(unicode_width::UnicodeWidthStr::width(keys.as_str())));
            Line::from(vec![
                Span::styled(
                    format!("{group}{pad_g}  "),
                    Style::default().fg(style::tui(style::FG3)),
                ),
                Span::styled(
                    format!("{keys}{pad_k}  "),
                    Style::default().fg(style::tui(style::CYAN)),
                ),
                Span::styled(
                    (*what).to_string(),
                    Style::default().fg(style::tui(style::FG2)),
                ),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" keys — any key closes ")
        .border_style(Style::default().fg(style::tui(style::BORDER)));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

const fn centered(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Every action by name, filtered as you type.
fn render_palette(f: &mut Frame, query: &str, sel: usize, rows: &[(String, &'static str, Act)]) {
    let area = f.area();
    let w = 56u16.min(area.width.saturating_sub(2));
    let h = (rows.len() as u16 + 3).min(area.height.saturating_sub(2));
    let rect = centered(area, w, h);
    let key_w = rows
        .iter()
        .map(|(k, ..)| unicode_width::UnicodeWidthStr::width(k.as_str()))
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled("› ", Style::default().fg(style::tui(style::FG3))),
        Span::styled(
            query.to_string(),
            Style::default()
                .fg(style::tui(style::FG))
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    for (i, (key, what, _)) in rows.iter().enumerate() {
        let pad =
            " ".repeat(key_w.saturating_sub(unicode_width::UnicodeWidthStr::width(key.as_str())));
        let marker = if i == sel { "▸ " } else { "  " };
        let what_style = if i == sel {
            Style::default()
                .fg(style::tui(style::FG))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(style::tui(style::FG2))
        };
        lines.push(Line::from(vec![
            Span::styled(
                marker.to_string(),
                Style::default().fg(style::tui(style::CYAN)),
            ),
            Span::styled(
                format!("{key}{pad}  "),
                Style::default().fg(style::tui(style::CYAN)),
            ),
            Span::styled((*what).to_string(), what_style),
        ]));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" run — enter picks, esc closes ")
        .border_style(Style::default().fg(style::tui(style::BORDER)));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

// Truncate to a display-width budget, appending "..." when over. Width,
// not byte length: comparing `s.len()` to a cell budget over-truncated
// CJK/emoji text and threw off right-edge padding math downstream.
fn truncate_line(s: &str, max: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let target = max - 3;
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > target {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::{App, View};
    use crate::keymap::Keymap;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::BTreeMap;

    // The web app's copy of the keymap. Read as source text rather than
    // executed: the point is to catch one surface's bindings moving
    // without the other's, and the two are written in different
    // languages, so the string is the only shared artifact.
    const WEB_APP_JS: &str = include_str!("web/app.js");

    fn app(overrides: &[(&str, &str)]) -> App {
        let map: BTreeMap<String, String> = overrides
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        App::new(
            Vec::new(),
            crate::source::pick(crate::source::SourceKind::default()),
            &map,
        )
    }

    fn press(a: &mut App, code: KeyCode) {
        a.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn quit_key_exits_from_the_landing_view() {
        let mut a = app(&[]);
        press(&mut a, KeyCode::Char('q'));
        assert!(a.quit);
    }

    #[test]
    fn ctrl_c_quits_even_though_it_is_not_in_the_keymap() {
        let mut a = app(&[]);
        a.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(a.quit);
    }

    #[test]
    fn ctrl_c_still_quits_while_typing_a_search() {
        let mut a = app(&[]);
        press(&mut a, KeyCode::Char('/'));
        assert!(a.searching);
        a.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(a.quit, "ctrl-c must not be swallowed as a search character");
    }

    #[test]
    fn dashboard_key_round_trips() {
        let mut a = app(&[]);
        press(&mut a, KeyCode::Char('d'));
        assert!(a.view == View::Dashboard);
        press(&mut a, KeyCode::Char('d'));
        assert!(a.view == View::Projects);
    }

    #[test]
    fn a_rebound_key_drives_the_action_and_the_default_goes_quiet() {
        let mut a = app(&[("dashboard", "D")]);
        press(&mut a, KeyCode::Char('d'));
        assert!(
            a.view == View::Projects,
            "`d` was rebound away and must be inert"
        );
        press(&mut a, KeyCode::Char('D'));
        assert!(a.view == View::Dashboard);
    }

    #[test]
    fn arrows_and_enter_keep_working_when_their_letters_are_rebound() {
        let mut a = app(&[("back", "B"), ("open", "O")]);
        press(&mut a, KeyCode::Char('d'));
        assert!(a.view == View::Dashboard);
        // Esc is a fixed alias for Quit, which closes the dashboard.
        press(&mut a, KeyCode::Esc);
        assert!(a.view == View::Projects);
    }

    #[test]
    fn the_palette_opens_filters_and_runs_the_picked_action() {
        let mut a = app(&[]);
        press(&mut a, KeyCode::Char(':'));
        assert!(a.palette_open);
        for c in "dashboard".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        let rows = a.palette_rows();
        assert_eq!(rows.len(), 1, "expected one match, got {rows:?}");
        press(&mut a, KeyCode::Enter);
        assert!(!a.palette_open);
        assert!(
            a.view == View::Dashboard,
            "the palette must run what it highlighted"
        );
    }

    #[test]
    fn the_palette_reaches_actions_whose_key_was_rebound_away() {
        let mut a = app(&[("dashboard", "ctrl-d")]);
        press(&mut a, KeyCode::Char(':'));
        for c in "dashboard".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        press(&mut a, KeyCode::Enter);
        assert!(a.view == View::Dashboard);
    }

    #[test]
    fn esc_closes_the_palette_without_running_anything() {
        let mut a = app(&[]);
        press(&mut a, KeyCode::Char(':'));
        press(&mut a, KeyCode::Esc);
        assert!(!a.palette_open);
        assert!(a.view == View::Projects);
    }

    #[test]
    fn the_help_overlay_swallows_exactly_one_key() {
        let mut a = app(&[]);
        press(&mut a, KeyCode::Char('?'));
        assert!(a.keys_open);
        press(&mut a, KeyCode::Char('q'));
        assert!(!a.keys_open);
        assert!(
            !a.quit,
            "the key that closes the overlay must not also fire"
        );
    }

    #[test]
    fn a_bad_rebind_surfaces_a_warning_rather_than_failing_silently() {
        let a = app(&[("down", "not-a-key")]);
        assert!(
            a.warnings.iter().any(|w| w.contains("keys.down")),
            "{:?}",
            a.warnings
        );
    }

    /// Every `?` row is spelled identically on both surfaces. Pinning
    /// only the motion rows is how `PgDn / PgUp` came to be advertised
    /// in both overlays while only the TUI implemented it.
    #[test]
    fn the_whole_key_overlay_matches_the_web() {
        for (_, keys, _, in_web) in Keymap::default().help() {
            if !in_web {
                continue;
            }
            // Quoted, so the match is the whole cell rather than a
            // substring of a longer one: bare `contains("g / G")` is
            // still satisfied by `'gg / GG'`.
            assert!(
                WEB_APP_JS.contains(&format!("'{keys}'")),
                "src/web/app.js no longer lists `{keys}` — the two keymaps have drifted"
            );
        }
    }

    /// Keys the web `?` promises are keys the web actually handles.
    #[test]
    fn web_implements_the_keys_its_overlay_advertises() {
        for (key, what) in [
            ("PageDown", "page down"),
            ("PageUp", "page up"),
            ("Enter", "activate the focused control"),
        ] {
            assert!(
                WEB_APP_JS.contains(&format!("key === '{key}'")),
                "src/web/app.js advertises {what} but has no `{key}` handler"
            );
        }
    }

    /// `h` and `l` are motion in every web view, with no per-view
    /// exception.
    ///
    /// The dashboard used to bind them to its hour-histogram and
    /// log-scale toggles, which made the same keystroke mean different
    /// things depending on the screen. Those moved to `H` / `L`. One
    /// handler apiece is what "no exception" looks like in the source; a
    /// second occurrence means a view claimed the letter back.
    #[test]
    fn web_binds_h_and_l_only_as_motion() {
        for (key, what) in [
            ("'h'", "back"),
            ("'l'", "open the selected row"),
            ("'H'", "the dashboard's hour histogram"),
            ("'L'", "the dashboard's log-scale toggle"),
        ] {
            let n = WEB_APP_JS.matches(&format!("key === {key}")).count();
            assert_eq!(
                n, 1,
                "expected exactly one `key === {key}` handler ({what}), found {n}"
            );
        }
    }

    /// Click targets that aren't native controls keep a keyboard path:
    /// a new `onclick` on a bare div/span/th joins `ACTIVATABLE` or it
    /// is mouse-only again.
    #[test]
    fn non_native_click_targets_stay_keyboard_reachable() {
        let activatable = WEB_APP_JS
            .lines()
            .skip_while(|l| !l.contains("const ACTIVATABLE"))
            .take(3)
            .collect::<String>();
        for sel in [
            "th.sortable",
            "[data-msg-sort]",
            ".crumb",
            ".drop-item",
            "tr.clickable",
            "[data-day]",
            "[data-model]",
        ] {
            assert!(
                activatable.contains(sel),
                "`{sel}` is clickable but dropped out of ACTIVATABLE — it is mouse-only again"
            );
        }
    }

    /// Roving tabindex: a 371-cell heatmap as tab stops buries the page.
    #[test]
    fn big_groups_rove_instead_of_becoming_tab_stops() {
        for marker in ["ROVE_GROUPS", "'.heatmap'", "'.hist-wrap'", "'.pie-wrap'"] {
            assert!(
                WEB_APP_JS.contains(marker),
                "src/web/app.js lost `{marker}`"
            );
        }
        assert!(
            WEB_APP_JS.contains("tabindex=\"-1\""),
            "focusable cells must start at tabindex -1 and be promoted by retab()"
        );
    }
}
