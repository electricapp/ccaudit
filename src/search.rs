use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

pub struct Searcher {
    matcher: Matcher,
}

impl Searcher {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
        }
    }

    pub fn score(&mut self, query: &str, haystack: &str) -> Option<u32> {
        if query.is_empty() {
            return Some(0);
        }
        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut buf = Vec::new();
        pattern.score(
            nucleo_matcher::Utf32Str::new(haystack, &mut buf),
            &mut self.matcher,
        )
    }

    pub fn matches(&mut self, query: &str, haystack: &str) -> bool {
        self.score(query, haystack).is_some()
    }
}
