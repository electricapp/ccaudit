use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

pub struct Searcher {
    matcher: Matcher,
    // Compiled pattern + the query it was compiled from. The TUI calls
    // `score()` once per candidate per keystroke (~hundreds of calls per
    // letter), but the query string is the same across all of them — so
    // we cache the parsed Pattern and rebuild only when the query changes.
    cached: Option<(String, Pattern)>,
    // Reusable Utf32 conversion buffer to avoid allocating a Vec<char>
    // on every score() call.
    buf: Vec<char>,
}

impl Default for Searcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Searcher {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            cached: None,
            buf: Vec::new(),
        }
    }

    pub fn score(&mut self, query: &str, haystack: &str) -> Option<u32> {
        if query.is_empty() {
            return Some(0);
        }
        let pattern = match &self.cached {
            Some((q, p)) if q == query => p,
            _ => {
                let p = Pattern::new(
                    query,
                    CaseMatching::Ignore,
                    Normalization::Smart,
                    AtomKind::Fuzzy,
                );
                self.cached = Some((query.to_string(), p));
                // Safe: we just inserted Some.
                &self.cached.as_ref()?.1
            }
        };
        self.buf.clear();
        pattern.score(
            nucleo_matcher::Utf32Str::new(haystack, &mut self.buf),
            &mut self.matcher,
        )
    }

    pub fn matches(&mut self, query: &str, haystack: &str) -> bool {
        self.score(query, haystack).is_some()
    }
}
