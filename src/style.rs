// Shared design tokens for TUI and web.
//
// One palette lives here and nowhere else. TUI reads the RGB tuples
// directly into `ratatui::Color::Rgb(...)`; web renders them into CSS
// custom properties at `generate()` time. Add a color once, both
// surfaces pick it up.
//
// Tokens come in two layers:
//   • Core palette (BG/FG/ACCENT/CYAN/GREEN/YELLOW/MAGENTA/RED/BORDER)
//   • Semantic aliases (K_USER, K_ASSISTANT, etc.) that point at the
//     core tokens. UI code references the semantic name so we can
//     retheme without touching every call site.
//
// All values are `Rgb` tuples (u8, u8, u8). Both conversion helpers
// (`tui`, `css_hex`) are `const` so tokens can live in consts.
pub type Rgb = (u8, u8, u8);

// ── Core palette ──

// "Embers" — warm graphite base with a signature coral accent. Slight
// warmth in the BG/FG channels keeps the dark theme looking crafted
// rather than "default terminal". ACCENT is the only loud color; it
// should appear on focus rings, active buttons, and key hover states,
// not on every heading.
pub const BG: Rgb = (0x14, 0x11, 0x0e);
pub const BG2: Rgb = (0x1c, 0x18, 0x14);
pub const BG3: Rgb = (0x26, 0x22, 0x1c);

pub const FG: Rgb = (0xeb, 0xe4, 0xd4);
pub const FG2: Rgb = (0x8a, 0x80, 0x72);
pub const FG3: Rgb = (0x4c, 0x45, 0x38);

pub const ACCENT: Rgb = (0xff, 0x7a, 0x4d);
pub const CYAN: Rgb = (0x6a, 0xd1, 0xbf);
pub const GREEN: Rgb = (0xb8, 0xd6, 0x7e);
pub const YELLOW: Rgb = (0xe8, 0xb4, 0x55);
pub const MAGENTA: Rgb = (0xc7, 0xa6, 0xff);
pub const RED: Rgb = (0xf0, 0x70, 0x56);

pub const BORDER: Rgb = (0x2a, 0x25, 0x20);

// Row-selected background used by TUI lists. Not exposed on web yet.
pub const ROW_SEL_BG: Rgb = (28, 22, 18);
// Dim separator used in TUI message view between turns.
pub const SEPARATOR: Rgb = (48, 40, 32);
// Muted header color used in TUI dashboard section titles.
pub const DASH_HEADER: Rgb = (120, 108, 90);

// ── Semantic: message-kind colors ──
// Both TUI and web reference these by name. Change the mapping here
// and both surfaces update.

pub const K_USER: Rgb = CYAN;
pub const K_ASSISTANT: Rgb = GREEN;
pub const K_TOOLUSE: Rgb = YELLOW;
pub const K_TOOLRESULT: Rgb = FG3;
pub const K_THINKING: Rgb = MAGENTA;
pub const K_SYSTEM: Rgb = FG3;

// ── Conversions ──

/// `ratatui::style::Color::Rgb` constructor. Kept in this module so
/// TUI call sites don't sprinkle `Color::Rgb(...)` all over.
#[cfg(feature = "tui")]
#[inline]
pub const fn tui(rgb: Rgb) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(rgb.0, rgb.1, rgb.2)
}

/// `#rrggbb` string for embedding in CSS / SVG.
#[cfg(feature = "web")]
pub fn css_hex(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
}
