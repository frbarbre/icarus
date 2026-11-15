use crate::facts::PullRequest;
use std::collections::BTreeMap;

pub const RULE_PROMPT_VARIABLES: [&str; 9] = [
    "prNumber",
    "title",
    "author",
    "branch",
    "baseRef",
    "url",
    "repo",
    "sinceCommit",
    "worktreePath",
];

pub const ACTION_PROMPT_VARIABLES: [&str; 9] = [
    "branch",
    "baseRef",
    "worktreePath",
    "prNumber",
    "prUrl",
    "prTitle",
    "draftMode",
    "prInstructions",
    "templateRule",
];

pub type Variables = BTreeMap<String, String>;

pub fn review_prompt_variables(pr: &PullRequest, since_commit: Option<&str>) -> Variables {
    [
        ("prNumber", pr.number.to_string()),
        ("title", pr.title.clone()),
        (
            "author",
            pr.author.clone().unwrap_or_else(|| "unknown".to_owned()),
        ),
        ("branch", pr.head_ref_name.clone()),
        ("baseRef", pr.base_ref_name.clone()),
        ("url", pr.url.clone()),
        ("repo", pr.repo.slug()),
        (
            "sinceCommit",
            since_commit.map_or_else(|| pr.base_ref_name.clone(), str::to_owned),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

fn find_placeholder(
    text: &str,
    start: usize,
    allow_padding: bool,
    is_name_char: fn(char) -> bool,
) -> Option<(usize, &str)> {
    if !text.get(start..)?.starts_with("{{") {
        return None;
    }
    let skip_space = |from: usize| -> usize {
        if !allow_padding {
            return from;
        }
        text[from..]
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map_or(text.len(), |(i, _)| from + i)
    };
    let offset = skip_space(start + 2);
    let name_len = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_name_char(*c))
        .map_or(text.len() - offset, |(i, _)| i);
    if name_len == 0 {
        return None;
    }
    let name = &text[offset..offset + name_len];
    let after = skip_space(offset + name_len);
    text[after..].starts_with("}}").then_some((after + 2, name))
}

fn replace_placeholders(
    template: &str,
    allow_padding: bool,
    is_name_char: fn(char) -> bool,
    lookup: impl Fn(&str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut index = 0;
    while index < template.len() {
        if let Some((end, name)) = find_placeholder(template, index, allow_padding, is_name_char) {
            match lookup(name) {
                Some(value) => out.push_str(&value),
                None => out.push_str(&template[index..end]),
            }
            index = end;
            continue;
        }
        let Some(c) = template[index..].chars().next() else {
            break;
        };
        out.push(c);
        index += c.len_utf8();
    }
    out
}

// Why verbatim: an unknown placeholder left in place makes a template typo visible in the prompt.
pub fn render_rule_prompt(template: &str, variables: &Variables) -> String {
    replace_placeholders(
        template,
        true,
        |c| c.is_ascii_alphabetic(),
        |name| {
            RULE_PROMPT_VARIABLES
                .contains(&name)
                .then(|| variables.get(name).cloned())
                .flatten()
        },
    )
}

pub fn render_action_prompt(template: &str, variables: &Variables) -> String {
    let filled = replace_placeholders(
        template,
        false,
        |c| c.is_ascii_alphanumeric() || c == '_',
        |name| variables.get(name).cloned(),
    );
    collapse_blank_lines(&filled).trim().to_owned()
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0usize;
    for c in text.chars() {
        if c == '\n' {
            run += 1;
            continue;
        }
        push_newlines(&mut out, run);
        run = 0;
        out.push(c);
    }
    push_newlines(&mut out, run);
    out
}

fn push_newlines(out: &mut String, run: usize) {
    let count = if run >= 3 { 2 } else { run };
    out.extend(std::iter::repeat_n('\n', count));
}

pub fn review_workspace_name(pr: &PullRequest) -> String {
    let mut slug = String::new();
    let mut in_gap = false;
    for c in pr.title.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
            in_gap = false;
        } else if !in_gap {
            slug.push('-');
            in_gap = true;
        }
    }
    let trimmed = slug.strip_prefix('-').unwrap_or(&slug);
    let trimmed = trimmed.strip_suffix('-').unwrap_or(trimmed);
    let cut: String = trimmed.chars().take(40).collect();
    if cut.is_empty() {
        format!("review-{}", pr.number)
    } else {
        format!("review-{}-{cut}", pr.number)
    }
}
