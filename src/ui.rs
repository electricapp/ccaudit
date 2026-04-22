use crate::parse::{MessageKind, Project};
use crate::report::fmt::{format_cost, format_datetime, format_datetime_short, format_number};
use crate::search::Searcher;
use crate::style;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use std::collections::HashMap;
use std::time::Duration;

// Pre-allocated spaces slab; slice out any indent width up to 64 chars
// with no runtime allocation. Used by render_messages on every redraw.
const SPACES: &str = "                                                                ";

#[derive(PartialEq)]
enum View {
    Projects,
    Sessions,
    Messages,
    Dashboard,
}

/// Set when the user asks the TUI to hand off to another program (e.g.
/// `c` → resume a Claude Code session, `o` → launch the web view). The
/// TUI quits cleanly and `main.rs` runs the action after terminal
/// teardown, so the new process inherits a clean stdio.
pub enum PostAction {
    Resume(String),
    OpenWeb,
}

pub struct App {
    projects: Vec<Project>,
    searcher: Searcher,
    view: View,
    search_query: String,
    searching: bool,
    project_state: ListState,
    session_state: ListState,
    message_scroll: u16,
    message_max_scroll: u16,
    selected_project: Option<usize>,
    selected_session: Option<usize>,
    filtered_projects: Vec<usize>,
    filtered_sessions: Vec<usize>,
    dash_scroll: u16,
    dash_max_scroll: u16,
    session_detail_open: bool,
    pub post_action: Option<PostAction>,
    quit: bool,
    // Dashboard aggregations computed once at App::new(). Projects are
    // immutable for the TUI's lifetime, so recomputing per-frame was
    // pure waste (HashMap<String,_> allocating a fresh key per session
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

fn compute_dashboard(projects: &[Project]) -> DashboardAgg {
    let mut total_sessions = 0usize;
    let mut total_msgs = 0usize;
    let mut total_cost = 0.0f64;
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cache_w = 0u64;
    let mut total_cache_r = 0u64;
    let mut by_project: Vec<ProjectAgg> = Vec::with_capacity(projects.len());
    // Per-model accumulator: (sessions, messages, tokens, cost, duration_ms, latest_started_at).
    let mut by_model_map: HashMap<&str, (usize, usize, u64, f64, u64, String)> = HashMap::new();
    let mut top_sessions: Vec<TopSession> = Vec::new();

    for (pi, p) in projects.iter().enumerate() {
        let mut pc = 0.0f64;
        let mut pm = 0usize;
        let mut ptok = 0u64;
        let mut pdur = 0u64;
        let mut plast = String::new();
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
            total_msgs += s.messages.len();
            total_cost += cost;
            total_input += s.total_input_tokens;
            total_output += s.total_output_tokens;
            total_cache_w += s.total_cache_create;
            total_cache_r += s.total_cache_read;
            pc += cost;
            pm += s.messages.len();
            ptok += stok;
            pdur += sdur;
            if s.started_at.is_some() && started > plast {
                plast.clone_from(&started);
            }

            // Borrow-keyed HashMap: model names are a tiny closed set
            // (~5 unique), no String allocation per session.
            let model = s.model.as_deref().unwrap_or("unknown");
            let entry = by_model_map
                .entry(model)
                .or_insert_with(|| (0, 0, 0, 0.0, 0, String::new()));
            entry.0 += 1;
            entry.1 += s.messages.len();
            entry.2 += stok;
            entry.3 += cost;
            entry.4 += sdur;
            if s.started_at.is_some() && started > entry.5 {
                entry.5.clone_from(&started);
            }

            top_sessions.push(TopSession {
                pi,
                si,
                cost,
                msgs: s.messages.len(),
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
            last_active: plast,
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
            last_active: last,
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
    pub fn new(projects: Vec<Project>) -> Self {
        let count = projects.len();
        let filtered: Vec<usize> = (0..count).collect();
        let mut state = ListState::default();
        if count > 0 {
            state.select(Some(0));
        }
        let dashboard = compute_dashboard(&projects);
        Self {
            projects,
            searcher: Searcher::new(),
            view: View::Projects,
            search_query: String::new(),
            searching: false,
            project_state: state,
            session_state: ListState::default(),
            message_scroll: 0,
            message_max_scroll: 0,
            selected_project: None,
            selected_session: None,
            filtered_projects: filtered,
            filtered_sessions: vec![],
            dash_scroll: 0,
            dash_max_scroll: 0,
            session_detail_open: false,
            post_action: None,
            quit: false,
            dashboard,
        }
    }

    pub fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> std::io::Result<()> {
        while !self.quit {
            let _ = terminal.draw(|f| self.render(f))?;
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    self.handle_key(key);
                }
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: event::KeyEvent) {
        if self.searching {
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
                KeyCode::Char(c) => {
                    self.search_query.push(c);
                    self.update_filter();
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => match self.view {
                View::Projects => self.quit = true,
                View::Dashboard => self.view = View::Projects,
                View::Sessions => {
                    self.view = View::Projects;
                    self.selected_project = None;
                    self.search_query.clear();
                    self.update_filter();
                }
                View::Messages => {
                    self.view = View::Sessions;
                    self.selected_session = None;
                    self.message_scroll = 0;
                }
            },
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            // `c` on Sessions or Messages view → resume the highlighted
            // session in Claude Code. Handed off to main.rs after the
            // TUI tears down so `claude` gets a clean terminal.
            KeyCode::Char('c') => {
                if let Some(id) = self.current_session_id() {
                    self.post_action = Some(PostAction::Resume(id));
                    self.quit = true;
                }
            }
            // `o` → regenerate the web bundle and open it in a browser.
            // Uses `h` in claude-code-log but `h` is already vim-left here.
            KeyCode::Char('o') => {
                self.post_action = Some(PostAction::OpenWeb);
                self.quit = true;
            }
            KeyCode::Tab if self.view == View::Sessions => {
                self.session_detail_open = !self.session_detail_open;
            }
            KeyCode::Char('d') => {
                if self.view == View::Dashboard {
                    self.view = View::Projects;
                } else {
                    self.view = View::Dashboard;
                    self.dash_scroll = 0;
                }
            }
            KeyCode::Char('/') => {
                self.searching = true;
                self.search_query.clear();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(),
            KeyCode::Char('g') => self.move_top(),
            KeyCode::Char('G') => self.move_bottom(),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.enter(),
            KeyCode::Char('h') | KeyCode::Left => self.back(),
            KeyCode::PageDown => {
                if self.view == View::Messages {
                    self.message_scroll = self
                        .message_scroll
                        .saturating_add(20)
                        .min(self.message_max_scroll);
                } else if self.view == View::Dashboard {
                    self.dash_scroll = self
                        .dash_scroll
                        .saturating_add(20)
                        .min(self.dash_max_scroll);
                }
            }
            KeyCode::PageUp => {
                if self.view == View::Messages {
                    self.message_scroll = self.message_scroll.saturating_sub(20);
                } else if self.view == View::Dashboard {
                    self.dash_scroll = self.dash_scroll.saturating_sub(20);
                }
            }
            _ => {}
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

    fn move_top(&mut self) {
        match self.view {
            View::Projects => self.project_state.select(Some(0)),
            View::Sessions => self.session_state.select(Some(0)),
            View::Messages => self.message_scroll = 0,
            View::Dashboard => self.dash_scroll = 0,
        }
    }

    fn move_bottom(&mut self) {
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
            View::Projects => {
                if let Some(i) = self.project_state.selected() {
                    if let Some(&pi) = self.filtered_projects.get(i) {
                        self.selected_project = Some(pi);
                        self.view = View::Sessions;
                        self.search_query.clear();
                        self.update_filter();
                        self.session_state.select(Some(0));
                    }
                }
            }
            View::Sessions => {
                if let Some(i) = self.session_state.selected() {
                    if let Some(&si) = self.filtered_sessions.get(i) {
                        self.selected_session = Some(si);
                        self.view = View::Messages;
                        self.message_scroll = 0;
                    }
                }
            }
            View::Messages | View::Dashboard => {}
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
                self.selected_session = None;
                self.message_scroll = 0;
            }
        }
    }

    fn current_session_id(&self) -> Option<String> {
        match self.view {
            View::Sessions => {
                let pi = self.selected_project?;
                let i = self.session_state.selected()?;
                let &si = self.filtered_sessions.get(i)?;
                Some(self.projects[pi].sessions[si].id.clone())
            }
            View::Messages => {
                let pi = self.selected_project?;
                let si = self.selected_session?;
                Some(self.projects[pi].sessions[si].id.clone())
            }
            _ => None,
        }
    }

    fn update_filter(&mut self) {
        // Disjoint-field borrows (edition 2024): we read
        // self.search_query / self.searcher / self.projects while
        // writing self.filtered_projects / self.filtered_sessions.
        // No clone needed — previously we cloned the query on every
        // keystroke.
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
    }

    fn render_statusbar(&self, f: &mut Frame, area: Rect) {
        let (left, right) = match self.view {
            View::Projects => {
                let total_sessions: usize = self.projects.iter().map(|p| p.sessions.len()).sum();
                let total_cost: f64 = self
                    .projects
                    .iter()
                    .flat_map(|p| &p.sessions)
                    .map(|s| s.cost)
                    .sum();
                (
                    format!(
                        " {} projects | {} sessions | {}",
                        self.projects.len(),
                        format_number(total_sessions as u64),
                        format_cost(total_cost),
                    ),
                    String::from(
                        "j/k nav | / search | enter select | o open web | d dashboard | q quit ",
                    ),
                )
            }
            View::Sessions => {
                if let Some(pi) = self.selected_project {
                    let p = &self.projects[pi];
                    let cost: f64 = p.sessions.iter().map(|s| s.cost).sum();
                    (
                        format!(
                            " {} | {} sessions | {}",
                            p.name,
                            format_number(p.sessions.len() as u64),
                            format_cost(cost),
                        ),
                        String::from(
                            "j/k nav | / search | enter view | tab detail | c resume | o web | esc back ",
                        ),
                    )
                } else {
                    (String::new(), String::new())
                }
            }
            View::Messages => {
                if let (Some(pi), Some(si)) = (self.selected_project, self.selected_session) {
                    let s = &self.projects[pi].sessions[si];
                    let cost = s.cost;
                    (
                        format!(
                            " {} msgs | {} | {}",
                            format_number(s.messages.len() as u64),
                            s.model.as_deref().unwrap_or("?"),
                            format_cost(cost),
                        ),
                        String::from("j/k scroll | PgDn/PgUp page | c resume | esc back "),
                    )
                } else {
                    (String::new(), String::new())
                }
            }
            View::Dashboard => {
                let total_cost: f64 = self
                    .projects
                    .iter()
                    .flat_map(|p| &p.sessions)
                    .map(|s| s.cost)
                    .sum();
                (
                    format!(" dashboard | {} total", format_cost(total_cost)),
                    String::from("j/k scroll | d close | q quit "),
                )
            }
        };

        let padding_len = (area.width as usize)
            .saturating_sub(left.len())
            .saturating_sub(right.len());
        let bg = Style::default().bg(style::tui(style::BG2));
        let full_bar = Line::from(vec![
            Span::styled(left, bg.fg(style::tui(style::FG))),
            Span::styled(" ".repeat(padding_len), bg),
            Span::styled(right, bg.fg(style::tui(style::FG2))),
        ]);
        f.render_widget(Paragraph::new(full_bar), area);
    }

    fn render_projects(&mut self, f: &mut Frame, area: Rect) {
        // Layout: [search?] [header] [list]
        let search_h = if self.searching { 3 } else { 0 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(search_h),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);

        if self.searching {
            let input = Paragraph::new(format!(" {}", self.search_query))
                .block(Block::default().borders(Borders::ALL).title(" search "));
            f.render_widget(input, chunks[0]);
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
                let msg_count: u64 = p.sessions.iter().map(|s| s.messages.len() as u64).sum();
                let dur_ms: u64 = p
                    .sessions
                    .iter()
                    .filter_map(|s| match (s.started_at, s.ended_at) {
                        (Some(a), Some(b)) if b > a => {
                            Some((b - a).num_milliseconds().max(0) as u64)
                        }
                        _ => None,
                    })
                    .sum();
                let cost: f64 = p.sessions.iter().map(|s| s.cost).sum();
                let line = unified_row_line(
                    &date,
                    &p.name,
                    Some(p.sessions.len() as u64),
                    msg_count,
                    p.total_tokens,
                    cost,
                    dur_ms,
                );
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items).highlight_style(
            Style::default()
                .bg(style::tui(style::ROW_SEL_BG))
                .fg(style::tui(style::ACCENT))
                .add_modifier(Modifier::BOLD),
        );

        f.render_stateful_widget(list, chunks[2], &mut self.project_state);
    }

    fn render_sessions(&mut self, f: &mut Frame, area: Rect) {
        let Some(pi) = self.selected_project else {
            return;
        };

        // Vertical layout: [search (0|3)] [list] [detail (0|7)]
        let search_h = if self.searching { 3 } else { 0 };
        let detail_h = if self.session_detail_open { 7 } else { 0 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(search_h),
                Constraint::Min(0),
                Constraint::Length(detail_h),
            ])
            .split(area);

        if self.searching {
            let input = Paragraph::new(format!(" {}", self.search_query))
                .block(Block::default().borders(Borders::ALL).title(" search "));
            f.render_widget(input, chunks[0]);
        }

        // Re-layout to insert a one-line header above the list.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(search_h),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(detail_h),
            ])
            .split(area);
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
                let line = unified_row_line(
                    &date,
                    s.display_name(),
                    None, // per-session row: session count is degenerate
                    s.messages.len() as u64,
                    s.total_tokens(),
                    s.cost,
                    dur_ms,
                );
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items).highlight_style(
            Style::default()
                .bg(style::tui(style::ROW_SEL_BG))
                .fg(style::tui(style::ACCENT))
                .add_modifier(Modifier::BOLD),
        );

        f.render_stateful_widget(list, chunks[2], &mut self.session_state);

        if self.session_detail_open {
            self.render_session_detail(f, chunks[3], pi);
        }
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

        let session = &self.projects[pi].sessions[si];
        let mut lines: Vec<Line> = Vec::new();

        // Build the separator once per frame — it's identical for every
        // user message on this area.width, so rebuilding per message was
        // pure waste on long sessions at 20fps.
        let separator: String = "─".repeat(area.width as usize);
        let separator_style = Style::default().fg(style::tui(style::SEPARATOR));

        for msg in &session.messages {
            // Tag colors read from the shared `style` tokens — same
            // palette the web `.tg-*` CSS uses, so colors match across
            // TUI and web for each message kind.
            let (tag, tag_color) = match msg.kind {
                MessageKind::User => ("YOU", style::tui(style::K_USER)),
                MessageKind::Assistant => ("AI ", style::tui(style::K_ASSISTANT)),
                MessageKind::ToolUse => (">>>", style::tui(style::K_TOOLUSE)),
                MessageKind::ToolResult => ("<<<", style::tui(style::K_TOOLRESULT)),
                MessageKind::Thinking => ("...", style::tui(style::K_THINKING)),
                MessageKind::System => ("SYS", style::tui(style::K_SYSTEM)),
            };

            if msg.kind == MessageKind::User {
                lines.push(Line::from(Span::styled(
                    separator.as_str(),
                    separator_style,
                )));
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
            let tag_w = tag_text.chars().count();
            let body_color = if msg.kind == MessageKind::ToolResult {
                style::tui(style::FG3)
            } else {
                style::tui(style::FG)
            };
            let aw = area.width as usize;
            // 1 leading space + content + fill + tag (fixed right-edge)
            let content_w = aw.saturating_sub(tag_w + 2);

            let mut iter = msg.content.lines();
            if let Some(first) = iter.next() {
                let body = truncate_line(first, content_w);
                let pad = content_w.saturating_sub(body.chars().count());
                let spaces: &'static str = &SPACES[..pad.min(SPACES.len())];
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

        let total_lines = lines.len() as u16;
        self.message_max_scroll = total_lines.saturating_sub(area.height);

        let title = session.display_name();
        let truncated_title = if title.len() > 60 {
            format!(" {}... ", &title[..57])
        } else {
            format!(" {title} ")
        };

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::NONE)
                    .title(truncated_title)
                    .title_style(Style::default().fg(style::tui(style::FG3))),
            )
            .scroll((self.message_scroll, 0));

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
        // not the old K/M abbreviation, which dropped the thousands
        // separator inside the mantissa and read as a bare "12884.5M".
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

        let total_lines = lines.len() as u16;
        self.dash_max_scroll = total_lines.saturating_sub(area.height);

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

fn truncate_line(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let target = max - 3;
    let mut end = target;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}
