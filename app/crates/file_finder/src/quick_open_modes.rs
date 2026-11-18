use std::ops::Range;

use fuzzy_nucleo::PathMatch;

const PREVIEW_BEFORE_MAX: usize = 40;
const DEPENDENCY_FOLDERS: [&str; 6] = [
    "node_modules",
    ".venv",
    "venv",
    "site-packages",
    "vendor",
    ".git",
];

const TIER_COUNT: u32 = 15;
// Far above any fuzzy score, so the tier decides first and the score only orders within it.
const TIER_SCORE_STEP: f64 = 1e12;

/// The parts Orca tiers on: each word of the query, after its last `/`.
pub(crate) fn file_name_queries(path_query: &str) -> Vec<String> {
    path_query
        .split_whitespace()
        .map(|word| {
            let word = word.replace('\\', "/").to_lowercase();
            match word.rsplit_once('/') {
                Some((_, name)) => name.to_owned(),
                None => word,
            }
        })
        .filter(|word| !word.is_empty())
        .collect()
}

/// Orca's Quick Open tiers, lower first: the file name equals, starts with, contains or holds a
/// query word's letters in order, else only the path matches. In each tier gitignored files come
/// second and dependency folders last. With several words the best one counts, so "nix icon" still
/// puts `nix.svg` in tier 1.
pub(crate) fn rank_tier(name_queries: &[String], path: &str, ignored: bool) -> u32 {
    let path = path.replace('\\', "/").to_lowercase();
    let (folders, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
    let name_tier = name_queries
        .iter()
        .map(|query| {
            if name == query {
                0
            } else if name.starts_with(query.as_str()) {
                1
            } else if name.contains(query.as_str()) {
                2
            } else if is_subsequence(query, name) {
                3
            } else {
                4
            }
        })
        .min()
        .unwrap_or(0);
    let in_dependency = folders
        .split('/')
        .any(|component| DEPENDENCY_FOLDERS.contains(&component));
    let placement = if in_dependency { 2 } else { u32::from(ignored) };
    name_tier * 3 + placement
}

fn is_subsequence(query: &str, text: &str) -> bool {
    let mut query = query.chars().peekable();
    for character in text.chars() {
        if query.peek() == Some(&character) {
            query.next();
        }
    }
    query.peek().is_none()
}

pub(crate) fn apply_rank_tier(name_queries: &[String], ignored: bool, path_match: &mut PathMatch) {
    if path_match.score.is_infinite() {
        return;
    }
    let tier = rank_tier(name_queries, path_match.path.as_unix_str(), ignored);
    path_match.score += f64::from(TIER_COUNT - tier) * TIER_SCORE_STEP;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineTarget {
    pub line: u32,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueryMode {
    Files,
    Text(String),
    GoToLine(Option<LineTarget>),
}

pub(crate) fn parse_query_mode(raw_query: &str) -> QueryMode {
    if let Some(text) = parse_text_query(raw_query) {
        return QueryMode::Text(text.to_owned());
    }
    match parse_go_to_line(raw_query) {
        Some(target) => QueryMode::GoToLine(target),
        None => QueryMode::Files,
    }
}

pub(crate) fn parse_text_query(raw_query: &str) -> Option<&str> {
    raw_query.strip_prefix('%').map(str::trim_start)
}

/// `Some(None)` is a bare `:` still waiting for a line number.
pub(crate) fn parse_go_to_line(raw_query: &str) -> Option<Option<LineTarget>> {
    let rest = raw_query.trim().strip_prefix(':')?;
    if rest.is_empty() {
        return Some(None);
    }
    let (line, column) = match rest.split_once(':') {
        Some((line, column)) => (line, Some(column)),
        None => (rest, None),
    };
    let parse_number = |text: &str| {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse::<u32>().ok().filter(|number| *number >= 1)
    };
    let line = parse_number(line)?;
    let column = match column {
        Some(column) => Some(parse_number(column)?),
        None => None,
    };
    Some(Some(LineTarget { line, column }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatchPreview {
    pub before: String,
    pub matched: String,
    pub after: String,
}

impl MatchPreview {
    pub fn new(line: &str, match_range: Range<usize>) -> Self {
        let valid = match_range.start <= match_range.end
            && match_range.end <= line.len()
            && line.is_char_boundary(match_range.start)
            && line.is_char_boundary(match_range.end);
        if !valid {
            return Self {
                before: line.trim_start().to_owned(),
                matched: String::new(),
                after: String::new(),
            };
        }
        let before = line[..match_range.start].trim_start();
        let before_chars = before.chars().count();
        let before = if before_chars > PREVIEW_BEFORE_MAX {
            let kept = before
                .chars()
                .skip(before_chars - (PREVIEW_BEFORE_MAX - 1))
                .collect::<String>();
            format!("…{kept}")
        } else {
            before.to_owned()
        };
        Self {
            before,
            matched: line[match_range.clone()].to_owned(),
            after: line[match_range.end..].to_owned(),
        }
    }

    pub fn text_and_match_range(&self) -> (String, Range<usize>) {
        let text = format!("{}{}{}", self.before, self.matched, self.after);
        let start = self.before.len();
        (text, start..start + self.matched.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_tier_orders_by_how_the_file_name_matches() {
        let queries = file_name_queries("Button");
        let tier = |path: &str| rank_tier(&queries, path, false);
        assert_eq!(tier("src/button"), 0);
        assert_eq!(rank_tier(&queries, "dist/button", true), 1);
        assert_eq!(tier("node_modules/ui/button"), 2);
        assert_eq!(rank_tier(&queries, "node_modules/ui/button", true), 2);
        assert_eq!(tier("src/button_group.rs"), 3);
        assert_eq!(tier("src/my_button_test.rs"), 6);
        assert_eq!(tier("src/b_u_t_t_o_n.rs"), 9);
        assert_eq!(tier("button/index.ts"), 12);
        assert_eq!(tier("vendor/button/index.ts"), 14);
    }

    #[test]
    fn rank_tier_reads_the_name_part_of_each_word() {
        assert_eq!(file_name_queries("src/Pla  live"), ["pla", "live"]);
        let queries = file_name_queries("nix icon");
        assert_eq!(rank_tier(&queries, "assets/icons/nix.svg", false), 3);
        assert_eq!(rank_tier(&queries, "zed/app-icon-nightly@2x.png", false), 6);
    }

    #[test]
    fn apply_rank_tier_puts_the_tier_before_the_fuzzy_score() {
        let path_match = |path: &str, score: f64| PathMatch {
            score,
            positions: Vec::new(),
            worktree_id: 0,
            path: std::sync::Arc::from(util::rel_path::rel_path(path)),
            path_prefix: util::rel_path::RelPath::empty_arc(),
            is_dir: false,
            distance_to_relative_ancestor: 0,
        };
        let queries = file_name_queries("main");
        let mut strong_path_hit = path_match("main/lib.rs", 900.0);
        let mut weak_name_hit = path_match("src/main.rs", 1.0);
        apply_rank_tier(&queries, false, &mut strong_path_hit);
        apply_rank_tier(&queries, false, &mut weak_name_hit);
        assert!(weak_name_hit.score > strong_path_hit.score);
    }

    #[test]
    fn go_to_line_reads_a_line_and_an_optional_column() {
        assert_eq!(
            parse_go_to_line(":12"),
            Some(Some(LineTarget {
                line: 12,
                column: None
            }))
        );
        assert_eq!(
            parse_go_to_line(" :12:4 "),
            Some(Some(LineTarget {
                line: 12,
                column: Some(4)
            }))
        );
        assert_eq!(parse_go_to_line(":"), Some(None));
    }

    #[test]
    fn go_to_line_leaves_file_queries_and_bad_numbers_alone() {
        assert_eq!(parse_go_to_line("scope.py:12"), None);
        assert_eq!(parse_go_to_line(":abc"), None);
        assert_eq!(parse_go_to_line(":0"), None);
        assert_eq!(parse_go_to_line(":12:0"), None);
        assert_eq!(parse_go_to_line(":12:"), None);
        assert_eq!(parse_go_to_line(":+3"), None);
    }

    #[test]
    fn text_query_reads_the_text_after_a_leading_percent() {
        assert_eq!(parse_text_query("%he"), Some("he"));
        assert_eq!(parse_text_query("% TYPE_CHECKING "), Some("TYPE_CHECKING "));
        assert_eq!(parse_text_query("%"), Some(""));
    }

    #[test]
    fn text_query_leaves_a_file_query_alone() {
        assert_eq!(parse_text_query("base.py"), None);
        assert_eq!(parse_text_query("a%b"), None);
    }

    #[test]
    fn query_mode_prefers_text_then_line_then_files() {
        assert_eq!(parse_query_mode("%:12"), QueryMode::Text(":12".into()));
        assert_eq!(
            parse_query_mode(":3"),
            QueryMode::GoToLine(Some(LineTarget {
                line: 3,
                column: None
            }))
        );
        assert_eq!(parse_query_mode("main.rs:3"), QueryMode::Files);
    }

    #[test]
    fn preview_splits_the_line_around_the_match_and_drops_indentation() {
        let line = "    if TYPE_he:";
        assert_eq!(
            MatchPreview::new(line, 12..14),
            MatchPreview {
                before: "if TYPE_".into(),
                matched: "he".into(),
                after: ":".into(),
            }
        );
    }

    #[test]
    fn preview_cuts_long_text_before_the_match_so_the_match_stays_visible() {
        let line = format!("{}he", "x".repeat(60));
        let preview = MatchPreview::new(&line, 60..62);
        assert!(preview.before.starts_with('…'));
        assert!(preview.before.chars().count() < 45);
        assert_eq!(preview.matched, "he");
    }

    #[test]
    fn preview_with_an_out_of_range_match_shows_the_line() {
        let preview = MatchPreview::new("  abc", 4..9);
        assert_eq!(preview.before, "abc");
        assert_eq!(preview.matched, "");
    }

    #[test]
    fn preview_text_highlights_the_match() {
        let preview = MatchPreview::new("let héllo = 1", 4..10);
        let (text, range) = preview.text_and_match_range();
        assert_eq!(&text[range], "héllo");
    }
}
