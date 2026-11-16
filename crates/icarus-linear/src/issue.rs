use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{LinearError, Result};

pub const DESCRIPTION_MAX_CHARS: usize = 1000;
pub const BATCH_MAX: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueState {
    pub name: String,
    /// Linear's workflow type: triage, backlog, unstarted, started, completed or canceled.
    #[serde(rename = "type")]
    pub kind: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssuePerson {
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueLabel {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    #[serde(default)]
    pub description_truncated: bool,
    pub state: Option<IssueState>,
    /// 0 none, 1 urgent, 2 high, 3 medium, 4 low.
    pub priority: Option<u8>,
    pub priority_label: Option<String>,
    pub assignee: Option<IssuePerson>,
    #[serde(default)]
    pub labels: Vec<IssueLabel>,
    pub url: String,
    pub branch_name: Option<String>,
    pub team_key: Option<String>,
    pub updated_at: Option<String>,
}

impl Issue {
    /// What a card shows; a change here is worth telling clients about.
    pub fn card_fingerprint(&self) -> (String, Option<String>, Vec<String>) {
        (
            self.title.clone(),
            self.state.as_ref().map(|s| s.name.clone()),
            self.labels.iter().map(|l| l.name.clone()).collect(),
        )
    }
}

/// `abc-12` → `ABC-12`; anything that isn't `TEAM-number` is rejected.
pub fn normalize_identifier(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let invalid = || LinearError::InvalidIdentifier(trimmed.to_owned());
    let (team, number) = trimmed.split_once('-').ok_or_else(invalid)?;
    let team_ok = team.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && team.len() <= 10
        && team.chars().all(|c| c.is_ascii_alphanumeric());
    let number_ok =
        !number.is_empty() && number.len() <= 9 && number.chars().all(|c| c.is_ascii_digit());
    if !team_ok || !number_ok {
        return Err(invalid());
    }
    Ok(format!("{}-{number}", team.to_ascii_uppercase()))
}

const ISSUE_FIELDS: &str = "fragment IssueFields on Issue { id identifier title description priority priorityLabel url branchName updatedAt state { name type color } assignee { name displayName avatarUrl } labels(first: 20) { nodes { name color } } team { key } }";

/// One query for several issues: an aliased `issue(id:)` per identifier, which Linear resolves by identifier too.
pub fn batch_query(identifiers: &[String]) -> (String, Value) {
    let mut params = Vec::new();
    let mut fields = Vec::new();
    let mut variables = serde_json::Map::new();
    for (index, identifier) in identifiers.iter().enumerate() {
        params.push(format!("$i{index}: String!"));
        fields.push(format!(
            "i{index}: issue(id: $i{index}) {{ ...IssueFields }}"
        ));
        variables.insert(format!("i{index}"), Value::String(identifier.clone()));
    }
    let query = format!(
        "query Issues({}) {{ {} }} {ISSUE_FIELDS}",
        params.join(", "),
        fields.join(" ")
    );
    (query, Value::Object(variables))
}

pub const SEARCH_MAX: usize = 50;

pub fn search_query(term: &str, first: usize) -> (String, Value) {
    (
        format!(
            "query SearchIssues($term: String!, $first: Int!) {{ searchIssues(term: $term, first: $first) {{ nodes {{ ...IssueFields }} }} }} {ISSUE_FIELDS}"
        ),
        serde_json::json!({ "term": term, "first": first.clamp(1, SEARCH_MAX) }),
    )
}

pub fn my_issues_query(first: usize) -> (String, Value) {
    (
        format!(
            "query MyIssues($first: Int!) {{ issues(first: $first, orderBy: updatedAt, filter: {{ assignee: {{ isMe: {{ eq: true }} }}, state: {{ type: {{ nin: [\"completed\", \"canceled\"] }} }} }}) {{ nodes {{ ...IssueFields }} }} }} {ISSUE_FIELDS}"
        ),
        serde_json::json!({ "first": first.clamp(1, SEARCH_MAX) }),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIssue {
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    priority: Option<f64>,
    priority_label: Option<String>,
    url: String,
    branch_name: Option<String>,
    updated_at: Option<String>,
    state: Option<IssueState>,
    assignee: Option<RawPerson>,
    labels: Option<RawLabels>,
    team: Option<RawTeam>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPerson {
    name: Option<String>,
    display_name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct RawLabels {
    nodes: Vec<IssueLabel>,
}

#[derive(Deserialize)]
struct RawTeam {
    key: String,
}

fn truncate(text: String) -> (String, bool) {
    match text.char_indices().nth(DESCRIPTION_MAX_CHARS) {
        Some((cut, _)) => (format!("{}…", text[..cut].trim_end()), true),
        None => (text, false),
    }
}

pub(crate) fn parse_issue(node: Value) -> Result<Issue> {
    let raw: RawIssue =
        serde_json::from_value(node).map_err(|e| LinearError::Malformed(e.to_string()))?;
    let (description, description_truncated) = match raw.description.filter(|d| !d.is_empty()) {
        Some(text) => {
            let (text, truncated) = truncate(text);
            (Some(text), truncated)
        }
        None => (None, false),
    };
    Ok(Issue {
        id: raw.id,
        identifier: raw.identifier,
        title: raw.title,
        description,
        description_truncated,
        state: raw.state,
        priority: raw.priority.map(|p| p.clamp(0.0, 4.0) as u8),
        priority_label: raw.priority_label,
        assignee: raw.assignee.and_then(|person| {
            Some(IssuePerson {
                name: person.display_name.or(person.name)?,
                avatar_url: person.avatar_url,
            })
        }),
        labels: raw.labels.map(|l| l.nodes).unwrap_or_default(),
        url: raw.url,
        branch_name: raw.branch_name.filter(|b| !b.is_empty()),
        team_key: raw.team.map(|t| t.key),
        updated_at: raw.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_normalized_and_validated() {
        assert_eq!(
            normalize_identifier(" abc-12 ").ok().as_deref(),
            Some("ABC-12")
        );
        assert_eq!(
            normalize_identifier("ENG2-7").ok().as_deref(),
            Some("ENG2-7")
        );
        for bad in [
            "", "ABC", "ABC-", "-12", "12-12", "ABC-1x", "AB C-1", "ABC-12-3",
        ] {
            assert!(
                normalize_identifier(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn the_batch_query_aliases_one_issue_field_per_identifier() {
        let (query, variables) = batch_query(&["ABC-1".into(), "ABC-2".into()]);
        assert!(query.starts_with("query Issues($i0: String!, $i1: String!) {"));
        assert!(query.contains("i0: issue(id: $i0) { ...IssueFields }"));
        assert!(query.contains("i1: issue(id: $i1) { ...IssueFields }"));
        assert!(query.contains("fragment IssueFields on Issue"));
        for field in [
            "identifier",
            "title",
            "description",
            "state { name type color }",
            "priority",
            "assignee { name displayName avatarUrl }",
            "labels(first: 20) { nodes { name color } }",
            "url",
            "branchName",
            "team { key }",
            "updatedAt",
        ] {
            assert!(query.contains(field), "query lacks {field}");
        }
        assert_eq!(
            variables,
            serde_json::json!({ "i0": "ABC-1", "i1": "ABC-2" })
        );
    }

    #[test]
    fn long_descriptions_are_truncated_on_a_char_boundary() {
        let node = serde_json::json!({
            "id": "u", "identifier": "ABC-1", "title": "t", "url": "https://linear.app/x/issue/ABC-1",
            "description": "é".repeat(DESCRIPTION_MAX_CHARS + 5),
        });
        let issue = parse_issue(node).expect("parse");
        assert!(issue.description_truncated);
        let description = issue.description.expect("description");
        assert_eq!(description.chars().count(), DESCRIPTION_MAX_CHARS + 1);
        assert!(description.ends_with('…'));
    }
}
