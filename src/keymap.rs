//! One keymap for the TUI and the generated web bundle, so a `[keys]`
//! rebind moves a key in both. Crossterm-free so the `web` feature can
//! use it; `ui.rs` adapts crossterm events into [`Key`].

use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Name {
    Char(char),
    Tab,
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Backspace,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub name: Name,
    pub ctrl: bool,
}

impl Key {
    pub const fn plain(name: Name) -> Self {
        Self { name, ctrl: false }
    }
    pub const fn ch(c: char) -> Self {
        Self::plain(Name::Char(c))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Down,
    Up,
    Top,
    Bottom,
    Open,
    Back,
    Quit,
    Search,
    Resume,
    Dashboard,
    Detail,
    Web,
    Help,
    Palette,
}

pub struct Binding {
    pub action: Action,
    /// Name under `[keys]` in the config file.
    pub id: &'static str,
    pub default: &'static str,
    /// Palette label, and the `?` text where the action stands alone.
    pub what: &'static str,
    /// False where the bundle has no equivalent.
    pub in_web: bool,
}

pub const BINDINGS: &[Binding] = &[
    Binding {
        action: Action::Down,
        id: "down",
        default: "j",
        what: "move down",
        in_web: true,
    },
    Binding {
        action: Action::Up,
        id: "up",
        default: "k",
        what: "move up",
        in_web: true,
    },
    Binding {
        action: Action::Top,
        id: "top",
        default: "g",
        what: "first row / top",
        in_web: true,
    },
    Binding {
        action: Action::Bottom,
        id: "bottom",
        default: "G",
        what: "last row / bottom",
        in_web: true,
    },
    Binding {
        action: Action::Open,
        id: "open",
        default: "l",
        what: "open the selected row",
        in_web: true,
    },
    Binding {
        action: Action::Back,
        id: "back",
        default: "h",
        what: "back",
        in_web: true,
    },
    Binding {
        action: Action::Quit,
        id: "quit",
        default: "q",
        what: "quit",
        in_web: true,
    },
    Binding {
        action: Action::Search,
        id: "search",
        default: "/",
        what: "filter the list",
        in_web: true,
    },
    Binding {
        action: Action::Resume,
        id: "resume",
        default: "c",
        what: "resume the session in Claude Code",
        in_web: true,
    },
    Binding {
        action: Action::Dashboard,
        id: "dashboard",
        default: "d",
        what: "toggle the dashboard",
        in_web: true,
    },
    Binding {
        action: Action::Detail,
        id: "detail",
        default: "tab",
        what: "expand session detail",
        in_web: false,
    },
    Binding {
        action: Action::Web,
        id: "web",
        default: "o",
        what: "regenerate the web bundle and open it",
        in_web: false,
    },
    Binding {
        action: Action::Help,
        id: "help",
        default: "?",
        what: "key reference",
        in_web: true,
    },
    Binding {
        action: Action::Palette,
        id: "palette",
        default: ":",
        what: "command palette",
        in_web: true,
    },
];

/// A cell in the `?` overlay: either a rebindable action's current key,
/// or a fixed key that was never rebindable to begin with.
pub enum K {
    A(Action),
    Lit(&'static str),
}

pub struct HelpRow {
    pub group: &'static str,
    pub keys: &'static [K],
    pub what: &'static str,
    /// False for TUI-only rows (`o`, session detail, process exit).
    pub in_web: bool,
}

pub const HELP: &[HelpRow] = &[
    HelpRow {
        group: "Navigate",
        keys: &[
            K::A(Action::Down),
            K::A(Action::Up),
            K::Lit("↑"),
            K::Lit("↓"),
        ],
        what: "move the selection, or scroll",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[K::A(Action::Top), K::A(Action::Bottom)],
        what: "first / last row, or top / bottom",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[K::A(Action::Open), K::Lit("→"), K::Lit("enter")],
        what: "open the selected row",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[
            K::A(Action::Back),
            K::Lit("←"),
            K::A(Action::Quit),
            K::Lit("esc"),
        ],
        what: "back",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[K::Lit("PgDn"), K::Lit("PgUp")],
        what: "page through a session or the dashboard",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[K::Lit("tab")],
        what: "step between tables, charts and controls",
        in_web: true,
    },
    HelpRow {
        group: "Find",
        keys: &[K::A(Action::Search)],
        what: "filter the list",
        in_web: true,
    },
    HelpRow {
        group: "Act",
        keys: &[K::A(Action::Resume)],
        what: "resume the session in Claude Code",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[K::A(Action::Detail)],
        what: "expand session detail",
        in_web: false,
    },
    HelpRow {
        group: "",
        keys: &[K::A(Action::Dashboard)],
        what: "toggle the dashboard",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[K::A(Action::Web)],
        what: "regenerate the web bundle and open it",
        in_web: false,
    },
    HelpRow {
        group: "",
        keys: &[K::A(Action::Palette)],
        what: "command palette",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[K::A(Action::Help)],
        what: "this list",
        in_web: true,
    },
    HelpRow {
        group: "",
        keys: &[K::A(Action::Quit), K::Lit("ctrl-c")],
        what: "quit",
        in_web: false,
    },
];

/// Resolved bindings: defaults with `[keys]` overrides applied.
pub struct Keymap {
    keys: Vec<(String, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new(&BTreeMap::new()).0
    }
}

impl Keymap {
    /// Defaults with `[keys]` applied, plus a warning per ignored entry
    /// so a typo is visible instead of silently doing nothing.
    pub fn new(overrides: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut keys: Vec<(String, Action)> = Vec::new();
        for b in BINDINGS {
            let spec = match overrides.get(b.id) {
                Some(s) if parse_key(s).is_none() => {
                    warnings.push(format!(
                        "keys.{}: `{s}` is not a key name, using `{}`",
                        b.id, b.default
                    ));
                    b.default.to_string()
                }
                Some(s) => s.clone(),
                None => b.default.to_string(),
            };
            keys.push((spec, b.action));
        }
        for (name, _) in overrides
            .iter()
            .filter(|(n, _)| !BINDINGS.iter().any(|b| b.id == **n))
        {
            warnings.push(format!("keys.{name}: no such action"));
        }
        for (i, (spec, _)) in keys.iter().enumerate() {
            let Some(j) = keys.iter().take(i).position(|(s, _)| s == spec) else {
                continue;
            };
            let (Some(first), Some(dup)) = (BINDINGS.get(j), BINDINGS.get(i)) else {
                continue;
            };
            warnings.push(format!(
                "keys.{} and keys.{} are both bound to `{spec}`; the first wins",
                first.id, dup.id
            ));
        }
        (Self { keys }, warnings)
    }

    /// First binding wins, so a duplicate never shadows an earlier action.
    pub fn action(&self, key: Key) -> Option<Action> {
        self.keys
            .iter()
            .find(|(spec, _)| parse_key(spec) == Some(key))
            .map(|(_, a)| *a)
    }

    /// Display form of whatever is bound to `action`.
    pub fn key_for(&self, action: Action) -> &str {
        self.keys
            .iter()
            .find(|(_, a)| *a == action)
            .map_or("", |(spec, _)| spec.as_str())
    }

    /// One `?` row as (group, keys, description, `in_web`).
    pub fn help(&self) -> Vec<(&'static str, String, &'static str, bool)> {
        HELP.iter()
            .map(|row| {
                let keys = row
                    .keys
                    .iter()
                    .map(|k| match k {
                        K::A(a) => self.key_for(*a).to_string(),
                        K::Lit(s) => (*s).to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" / ");
                (row.group, keys, row.what, row.in_web)
            })
            .collect()
    }

    /// Every action the palette can run, as (key, label, action).
    pub fn palette_entries(&self) -> Vec<(&str, &'static str, Action)> {
        BINDINGS
            .iter()
            .filter(|b| b.action != Action::Palette)
            .map(|b| (self.key_for(b.action), b.what, b.action))
            .collect()
    }

    /// User key -> the default key `app.js` is written against. Retired
    /// defaults map to "" so they stop firing.
    pub fn web_rebind(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for b in BINDINGS.iter().filter(|b| b.in_web) {
            let cur = self.key_for(b.action);
            if cur != b.default {
                let _ = out.insert(cur.to_string(), b.default.to_string());
            }
        }
        // Or `j` would still move down after being rebound away.
        for b in BINDINGS.iter().filter(|b| b.in_web) {
            let still_used = BINDINGS.iter().any(|o| self.key_for(o.action) == b.default);
            if !still_used && !out.contains_key(b.default) {
                let _ = out.insert(b.default.to_string(), String::new());
            }
        }
        out
    }
}

/// Parses a key spec (`j`, `?`, `tab`, `ctrl-k`). None for anything
/// unrecognized — callers treat that as a config error.
pub fn parse_key(spec: &str) -> Option<Key> {
    let (ctrl, rest) = match spec.strip_prefix("ctrl-") {
        Some(r) => (true, r),
        None => (false, spec),
    };
    let name = match rest {
        "tab" => Name::Tab,
        "enter" => Name::Enter,
        "esc" => Name::Esc,
        "space" => Name::Char(' '),
        "up" => Name::Up,
        "down" => Name::Down,
        "left" => Name::Left,
        "right" => Name::Right,
        "pgup" => Name::PageUp,
        "pgdn" => Name::PageDown,
        "home" => Name::Home,
        "end" => Name::End,
        "backspace" => Name::Backspace,
        _ => {
            let mut it = rest.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None;
            }
            Name::Char(c)
        }
    };
    Some(Key { name, ctrl })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn over(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn every_binding_has_a_parseable_default() {
        for b in BINDINGS {
            assert!(
                parse_key(b.default).is_some(),
                "keys.{} default `{}`",
                b.id,
                b.default
            );
        }
    }

    #[test]
    fn defaults_are_unique_and_resolve() {
        let (km, warnings) = Keymap::new(&BTreeMap::new());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(km.action(Key::ch('j')), Some(Action::Down));
        assert_eq!(km.action(Key::ch('?')), Some(Action::Help));
        assert_eq!(km.action(Key::plain(Name::Tab)), Some(Action::Detail));
    }

    #[test]
    fn an_override_moves_the_action_and_frees_the_old_key() {
        let (km, warnings) = Keymap::new(&over(&[("down", "n")]));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(km.action(Key::ch('n')), Some(Action::Down));
        assert_eq!(km.action(Key::ch('j')), None);
        assert_eq!(km.key_for(Action::Down), "n");
    }

    #[test]
    fn ctrl_chords_do_not_swallow_the_bare_letter() {
        let (km, _) = Keymap::new(&over(&[("palette", "ctrl-k")]));
        assert_eq!(
            km.action(Key {
                name: Name::Char('k'),
                ctrl: true
            }),
            Some(Action::Palette)
        );
        assert_eq!(km.action(Key::ch('k')), Some(Action::Up));
    }

    #[test]
    fn an_unparseable_override_warns_and_keeps_the_default() {
        let (km, warnings) = Keymap::new(&over(&[("down", "nope")]));
        assert_eq!(km.key_for(Action::Down), "j");
        assert!(
            warnings.iter().any(|w| w.contains("keys.down")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_unknown_action_name_warns() {
        let (_, warnings) = Keymap::new(&over(&[("dwon", "n")]));
        assert!(
            warnings.iter().any(|w| w.contains("no such action")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_collision_warns_and_leaves_the_first_action_reachable() {
        let (km, warnings) = Keymap::new(&over(&[("up", "j")]));
        assert!(
            warnings.iter().any(|w| w.contains("both bound")),
            "{warnings:?}"
        );
        assert_eq!(km.action(Key::ch('j')), Some(Action::Down));
    }

    #[test]
    fn help_rows_render_the_live_keys() {
        let (km, _) = Keymap::new(&over(&[("down", "n")]));
        assert_eq!(km.help().first().expect("help rows").1, "n / k / ↑ / ↓");
    }

    #[test]
    fn every_help_row_resolves_to_a_non_empty_key_list() {
        let km = Keymap::default();
        for (group, keys, what, _) in km.help() {
            assert!(!keys.is_empty(), "empty keys for {group}/{what}");
            assert!(!keys.contains("//"), "unresolved action in `{keys}`");
        }
    }

    #[test]
    fn tui_only_rows_are_flagged_out_of_the_web_overlay() {
        let km = Keymap::default();
        let row = |what: &str| km.help().into_iter().find(|r| r.2 == what).expect("row");
        assert!(
            !row("regenerate the web bundle and open it").3,
            "`o` is TUI-only"
        );
        assert!(!row("expand session detail").3, "`tab` is TUI-only");
        assert!(
            row("move the selection, or scroll").3,
            "motion is on both surfaces"
        );
    }

    #[test]
    fn web_rebind_maps_the_new_key_and_retires_the_old() {
        let (km, _) = Keymap::new(&over(&[("down", "n")]));
        let r = km.web_rebind();
        assert_eq!(r.get("n").map(String::as_str), Some("j"));
        assert_eq!(r.get("j").map(String::as_str), Some(""));
    }

    #[test]
    fn web_rebind_is_empty_when_nothing_was_rebound() {
        assert!(Keymap::default().web_rebind().is_empty());
    }

    #[test]
    fn tui_only_actions_stay_out_of_the_web_rebind() {
        let (km, _) = Keymap::new(&over(&[("web", "W")]));
        assert!(
            km.web_rebind().is_empty(),
            "`o` is TUI-only; the bundle has no binding to move"
        );
    }
}
