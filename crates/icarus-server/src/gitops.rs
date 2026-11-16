//! Source Control actions as RPCs: discard, stage, commit, push, pull and sync in a workspace's worktree.
//!
//! They run here because the server owns the worktree; the app and agents ask for them the same way.

use std::sync::Arc;

use icarus_git::ops::{self, CommitRequest, OpError, PushRequest};
use icarus_protocol::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::notes::Target;
use crate::server::{Server, params, rpc, to_value};

pub const GIT_CHANGED_TOPIC: &str = "git.changed";
const COMMITS_LIMIT: usize = 200;

fn git_error(error: OpError) -> RpcError {
    rpc("git_failed", error.message)
}

#[derive(Deserialize)]
struct Paths {
    #[serde(flatten)]
    target: Target,
    paths: Vec<String>,
}

#[derive(Deserialize)]
struct Commit {
    #[serde(flatten)]
    target: Target,
    #[serde(flatten)]
    request: CommitRequest,
}

#[derive(Deserialize)]
struct Push {
    #[serde(flatten)]
    target: Target,
    #[serde(flatten)]
    request: PushRequest,
}

impl Server {
    pub(crate) async fn git_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        let (workspace, value) = match method {
            "git.status" => {
                let target: Target = params(p)?;
                let workspace = self.note_workspace(&target)?;
                let path = workspace.path.clone();
                let state = blocking(move || ops::sync_state(&path)).await?;
                return to_value(state);
            }
            "git.commits" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(flatten)]
                    target: Target,
                    base: Option<String>,
                    limit: Option<usize>,
                }
                let p: P = params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                let path = workspace.path.clone();
                let base = p.base.or(workspace.base_ref.clone());
                let limit = p.limit.unwrap_or(COMMITS_LIMIT).clamp(1, 1000);
                let commits = tokio::task::spawn_blocking(move || {
                    icarus_git::branch_commits(&path, base, limit)
                })
                .await
                .map_err(|error| rpc("internal", error))?
                .map_err(|error| rpc("git_failed", format!("{error:#}")))?;
                return to_value(commits);
            }
            "git.commit_files" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(flatten)]
                    target: Target,
                    sha: String,
                }
                let p: P = params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                let path = workspace.path.clone();
                let files = tokio::task::spawn_blocking(move || {
                    icarus_git::commit_files(&path, p.sha.trim())
                })
                .await
                .map_err(|error| rpc("internal", error))?
                .map_err(|error| rpc("git_failed", format!("{error:#}")))?;
                return to_value(files);
            }
            "git.discard" => {
                let p: Paths = params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                let path = workspace.path.clone();
                let done = blocking(move || ops::discard(&path, &p.paths)).await?;
                let done: Vec<Value> = done
                    .into_iter()
                    .map(
                        |(path, kind)| json!({ "path": path, "kind": kind, "label": kind.label() }),
                    )
                    .collect();
                (workspace, json!({ "discarded": done }))
            }
            "git.discard_kind" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(flatten)]
                    target: Target,
                    path: String,
                }
                let p: P = params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                let root = workspace.path.clone();
                let path = p.path;
                let kind = blocking(move || {
                    let path = ops::checked_path(&root, &path)?;
                    ops::discard_kind(&root, &path)
                })
                .await?;
                return Ok(json!({ "kind": kind, "label": kind.map(|k| k.label()) }));
            }
            "git.stage" | "git.unstage" => {
                let p: Paths = params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                let path = workspace.path.clone();
                let stage = method == "git.stage";
                blocking(move || {
                    if stage {
                        ops::stage(&path, &p.paths)
                    } else {
                        ops::unstage(&path, &p.paths)
                    }
                })
                .await?;
                (workspace, json!({ "ok": true }))
            }
            "git.commit" => {
                let p: Commit = params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                let path = workspace.path.clone();
                let outcome = blocking(move || ops::commit(&path, &p.request)).await?;
                (workspace, to_value(outcome)?)
            }
            "git.push" => {
                let p: Push = params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                let path = workspace.path.clone();
                let state = blocking(move || ops::push(&path, p.request)).await?;
                (workspace, to_value(state)?)
            }
            "git.pull" | "git.sync" => {
                let target: Target = params(p)?;
                let workspace = self.note_workspace(&target)?;
                let path = workspace.path.clone();
                let sync = method == "git.sync";
                let state = blocking(move || {
                    if sync {
                        ops::sync(&path)
                    } else {
                        ops::pull(&path)
                    }
                })
                .await?;
                (workspace, to_value(state)?)
            }
            other => return Err(rpc("unknown_method", other)),
        };
        // Pushes and pulls move PR facts too, so the next GitHub read must not reuse a cached one.
        if matches!(method, "git.push" | "git.pull" | "git.sync" | "git.commit") {
            self.github.invalidate(&workspace.project_id);
        }
        self.broadcast(
            GIT_CHANGED_TOPIC,
            json!({ "workspace": workspace.id, "op": method.trim_start_matches("git.") }),
        );
        self.touch_activity(&workspace.id);
        Ok(value)
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, OpError> + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| rpc("internal", error))?
        .map_err(git_error)
}
