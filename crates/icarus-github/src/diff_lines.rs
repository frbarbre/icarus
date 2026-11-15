//! Which lines of a pull request GitHub accepts a line comment on: the PR's own diff hunks, per side.

use std::collections::{BTreeMap, BTreeSet};

use crate::client::GitHub;
use crate::error::Result;
use crate::review::Side;
use crate::runner::{Gh, GhRequest};
use icarus_rules::facts::RepoRef;
use regex::Regex;
use serde::Deserialize;

const MAX_PAGES: usize = 30;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileLines {
    pub right: BTreeSet<u32>,
    pub left: BTreeSet<u32>,
}

/// The lines of one file's unified diff that sit in a hunk, as added, removed or context lines.
pub fn commentable_lines(patch: &str) -> FileLines {
    let header = Regex::new(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@").expect("hunk regex");
    let mut lines = FileLines::default();
    let (mut left, mut right) = (0u32, 0u32);
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(captures) = header.captures(line) {
            left = captures[1].parse().unwrap_or(0);
            right = captures[2].parse().unwrap_or(0);
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                lines.right.insert(right);
                right += 1;
            }
            Some(b'-') => {
                lines.left.insert(left);
                left += 1;
            }
            Some(b'\\') => {}
            _ => {
                lines.right.insert(right);
                lines.left.insert(left);
                right += 1;
                left += 1;
            }
        }
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Every line of the range is in the PR diff.
    InDiff,
    /// The file is in the PR, but not these lines: GitHub only takes a file comment.
    OutsideDiff,
    /// The PR does not change this file at all.
    NotInPr,
    /// GitHub sent no patch for the file (binary or too large); let GitHub decide.
    Unknown,
}

/// The files a pull request changes, with their commentable lines when GitHub sent a patch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrDiff {
    pub files: BTreeMap<String, Option<FileLines>>,
}

impl PrDiff {
    pub fn place(&self, path: &str, side: Side, start: u32, end: u32) -> Placement {
        let Some(file) = self.files.get(path) else {
            return Placement::NotInPr;
        };
        let Some(lines) = file else {
            return Placement::Unknown;
        };
        let set = match side {
            Side::Right => &lines.right,
            Side::Left => &lines.left,
        };
        if (start.min(end)..=end.max(start)).all(|line| set.contains(&line)) {
            Placement::InDiff
        } else {
            Placement::OutsideDiff
        }
    }
}

/// GitHub's file-comment fallback body: the quoted lines, then the user's text.
pub fn file_comment_body(start: u32, end: u32, code: &[&str], body: &str) -> String {
    let label = if start == end {
        format!("Line {end}:")
    } else {
        format!("Lines {start}\u{2013}{end}:")
    };
    if code.is_empty() {
        return format!("{label}\n\n{body}");
    }
    let longest = code
        .iter()
        .map(|line| {
            line.split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or_default()
        })
        .max()
        .unwrap_or_default();
    let fence = "`".repeat((longest + 1).max(3));
    format!("{label}\n{fence}\n{}\n{fence}\n\n{body}", code.join("\n"))
}

#[derive(Deserialize)]
struct RawFile {
    filename: String,
    #[serde(default)]
    patch: Option<String>,
}

impl<G: Gh> GitHub<G> {
    pub async fn pr_diff(&self, repo: &RepoRef, number: u64) -> Result<PrDiff> {
        let mut diff = PrDiff::default();
        for page in 1..=MAX_PAGES {
            let files: Vec<RawFile> = self
                .rest_json(GhRequest::get(format!(
                    "repos/{}/{}/pulls/{number}/files?per_page=100&page={page}",
                    repo.owner, repo.name
                )))
                .await?;
            let count = files.len();
            for file in files {
                diff.files
                    .insert(file.filename, file.patch.as_deref().map(commentable_lines));
            }
            if count < 100 {
                break;
            }
        }
        Ok(diff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "@@ -1,4 +1,5 @@\n one\n-two\n+TWO\n+two and a half\n three\n four\n@@ -20,2 +21,3 @@ fn x()\n twenty\n+new\n twentyone\n\\ No newline at end of file";

    #[test]
    fn hunks_give_added_removed_and_context_lines_per_side() {
        let lines = commentable_lines(PATCH);
        assert_eq!(
            lines.right.iter().copied().collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 21, 22, 23]
        );
        assert_eq!(
            lines.left.iter().copied().collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 20, 21]
        );
    }

    #[test]
    fn a_range_must_be_wholly_inside_the_diff() {
        let mut diff = PrDiff::default();
        diff.files
            .insert("a.rs".into(), Some(commentable_lines(PATCH)));
        diff.files.insert("bin.png".into(), None);
        assert_eq!(diff.place("a.rs", Side::Right, 2, 5), Placement::InDiff);
        assert_eq!(
            diff.place("a.rs", Side::Right, 5, 21),
            Placement::OutsideDiff
        );
        assert_eq!(diff.place("a.rs", Side::Left, 20, 21), Placement::InDiff);
        assert_eq!(diff.place("bin.png", Side::Right, 1, 1), Placement::Unknown);
        assert_eq!(
            diff.place("other.rs", Side::Right, 1, 1),
            Placement::NotInPr
        );
    }

    #[test]
    fn file_comment_quotes_the_lines_in_a_long_enough_fence() {
        assert_eq!(
            file_comment_body(7, 7, &["let x = 1;"], "Why?"),
            "Line 7:\n```\nlet x = 1;\n```\n\nWhy?"
        );
        assert_eq!(
            file_comment_body(3, 4, &["a ```` b", "c"], "Hm"),
            "Lines 3\u{2013}4:\n`````\na ```` b\nc\n`````\n\nHm"
        );
    }
}
