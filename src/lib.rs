// Library facade for ccaudit. The binary (`src/main.rs`) and the
// `benches/bench.rs` benchmark (run via `cargo bench`) both consume the
// modules through this crate so internal paths stay one source of truth.
//
// Nothing here adds or hides API surface — `pub mod` mirrors what the
// binary previously declared via `mod ...;`. `#[allow(...)]` annotations
// are preserved verbatim from the binary's original module declarations.

pub mod cache;
pub mod cli;
pub mod parse;
pub mod report;
pub mod source;
pub mod style;

#[cfg(feature = "tui")]
pub mod search;
#[cfg(feature = "tui")]
#[allow(clippy::indexing_slicing)]
pub mod ui;

#[cfg(feature = "web")]
pub mod serve;
#[cfg(feature = "web")]
#[allow(clippy::indexing_slicing)]
pub mod web;
