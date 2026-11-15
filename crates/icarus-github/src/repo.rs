use crate::client::GitHub;
use crate::error::Result;
use crate::raw::Nodes;
use crate::runner::Gh;
use icarus_rules::facts::{MergeMethod, PullRequest, RepoRef};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMergeMetadata {
    pub viewer_default_method: Option<MergeMethod>,
    pub allowed_methods: Vec<MergeMethod>,
    pub auto_merge_allowed: Option<bool>,
    pub merge_queue_required: Option<bool>,
}

impl RepoMergeMetadata {
    pub fn default_method(&self) -> Option<MergeMethod> {
        self.viewer_default_method
            .filter(|method| self.allowed_methods.contains(method))
            .or_else(|| self.allowed_methods.first().copied())
    }

    pub fn apply(&self, pr: &mut PullRequest) {
        pr.merge_queue_required = self.merge_queue_required;
        if self.auto_merge_allowed.is_some() {
            pr.auto_merge_allowed = self.auto_merge_allowed;
        }
        if let Some(stack) = pr.stack.as_mut() {
            stack.merge_queue_required = self.merge_queue_required;
        }
    }
}

const QUERY: &str = "query RepoMergeMetadata($owner: String!, $name: String!, $branch: String!, $qualified: String!) {
  repository(owner: $owner, name: $name) {
    viewerDefaultMergeMethod mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed autoMergeAllowed
    mergeQueue(branch: $branch) { id }
    ref(qualifiedName: $qualified) { rules(first: 50) { nodes { type } } }
  }
}";

#[derive(Deserialize)]
struct Rule {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct Ref {
    #[serde(default)]
    rules: Option<Nodes<Rule>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    #[serde(default)]
    viewer_default_merge_method: Option<MergeMethod>,
    #[serde(default)]
    merge_commit_allowed: bool,
    #[serde(default)]
    squash_merge_allowed: bool,
    #[serde(default)]
    rebase_merge_allowed: bool,
    #[serde(default)]
    auto_merge_allowed: Option<bool>,
    #[serde(default)]
    merge_queue: Option<IgnoredAny>,
    #[serde(default, rename = "ref")]
    reference: Option<Ref>,
}

#[derive(Deserialize)]
struct Data {
    repository: Option<Raw>,
}

impl<G: Gh> GitHub<G> {
    pub async fn repo_merge_metadata(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<RepoMergeMetadata> {
        let data: Data = self
            .graphql_data(
                QUERY,
                json!({
                    "owner": repo.owner,
                    "name": repo.name,
                    "branch": branch,
                    "qualified": format!("refs/heads/{branch}"),
                }),
            )
            .await?;
        let raw = data.repository.ok_or_else(|| {
            crate::GitHubError::NotFound(format!("Could not find repository {}.", repo.slug()))
        })?;
        let mut allowed_methods = Vec::new();
        if raw.merge_commit_allowed {
            allowed_methods.push(MergeMethod::Merge);
        }
        if raw.squash_merge_allowed {
            allowed_methods.push(MergeMethod::Squash);
        }
        if raw.rebase_merge_allowed {
            allowed_methods.push(MergeMethod::Rebase);
        }
        let queue_rule = raw
            .reference
            .and_then(|reference| reference.rules)
            .is_some_and(|rules| rules.items().any(|rule| rule.kind == "MERGE_QUEUE"));
        Ok(RepoMergeMetadata {
            viewer_default_method: raw.viewer_default_merge_method,
            allowed_methods,
            auto_merge_allowed: raw.auto_merge_allowed,
            merge_queue_required: Some(raw.merge_queue.is_some() || queue_rule),
        })
    }
}
