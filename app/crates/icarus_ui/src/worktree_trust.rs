//! Worktrees Icarus created are trusted without asking, so language servers start right away.
//! Folders that aren't Icarus workspaces keep Zed's normal trust prompt.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use collections::HashSet;
use gpui::{App, Context, TaskExt as _, Window};
use gpui_tokio::Tokio;
use icarus_client::Client;
use project::trusted_worktrees::{PathTrust, TrustedWorktrees, TrustedWorktreesEvent};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::{AppConnection, server_home};

/// The restricted worktrees that live inside an Icarus workspace.
pub fn icarus_worktrees<Id: Copy>(restricted: &[(Id, &Path)], icarus_paths: &[PathBuf]) -> Vec<Id> {
    restricted
        .iter()
        .filter(|(_, path)| {
            icarus_paths
                .iter()
                .any(|icarus_path| path.starts_with(icarus_path))
        })
        .map(|(id, _)| *id)
        .collect()
}

pub(crate) async fn list_workspace_paths(client: Arc<Client>) -> Result<Vec<(String, PathBuf)>> {
    let workspaces = client
        .request("workspace.list", serde_json::json!({}))
        .await?;
    Ok(workspaces
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|workspace| {
            Some((
                workspace["id"].as_str()?.to_string(),
                PathBuf::from(workspace["path"].as_str()?),
            ))
        })
        .collect())
}

/// Trusts this workspace's restricted worktrees that are Icarus workspaces; returns whether any remain unknown.
fn trust_known(workspace: &Workspace, cx: &mut App) -> bool {
    let project = workspace.project().read(cx);
    if !project.is_local() {
        return false;
    }
    let worktree_store = project.worktree_store();
    let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
        return false;
    };
    let restricted = trusted_worktrees
        .read(cx)
        .restricted_worktrees(&worktree_store, cx);
    if restricted.is_empty() {
        return false;
    }
    let mut icarus_paths = cx
        .try_global::<AppConnection>()
        .map(|connection| connection.workspace_paths.clone())
        .unwrap_or_default();
    if let Some(store) = crate::servers::existing_store(cx) {
        icarus_paths.extend(store.read(cx).servers().iter().flat_map(|server| {
            server
                .workspace_paths()
                .into_iter()
                .map(Path::to_path_buf)
                .collect::<Vec<_>>()
        }));
    }
    let restricted: Vec<_> = restricted
        .iter()
        .map(|(id, path)| (*id, path.as_ref()))
        .collect();
    let matching = icarus_worktrees(&restricted, &icarus_paths);
    let unknown = matching.len() < restricted.len();
    if !matching.is_empty() {
        let paths: HashSet<PathTrust> = matching.into_iter().map(PathTrust::Worktree).collect();
        trusted_worktrees.update(cx, |trusted_worktrees, cx| {
            trusted_worktrees.trust(&worktree_store, paths, cx);
        });
    }
    unknown
}

fn trust_icarus_worktrees(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !trust_known(workspace, cx) {
        return;
    }
    // The cached list may predate a workspace the server just created, so ask again before the
    // user is left with a prompt for a folder Icarus made.
    let client = cx
        .try_global::<AppConnection>()
        .and_then(|connection| connection.client.clone());
    let listed = Tokio::spawn_result(cx, async move {
        let client = match client {
            Some(client) => client,
            None => {
                let home = server_home().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
                Client::connect(&home, "icarus-trust").await?.0
            }
        };
        list_workspace_paths(client).await
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let listed = match listed.await {
            Ok(listed) => listed,
            Err(error) => {
                log::debug!("icarus server unavailable for worktree trust: {error:#}");
                return anyhow::Ok(());
            }
        };
        workspace.update_in(cx, |workspace, window, cx| {
            cx.default_global::<AppConnection>().workspace_paths =
                listed.into_iter().map(|(_, path)| path).collect();
            trust_known(workspace, cx);
            workspace.show_worktree_trust_security_modal(false, window, cx);
        })
    })
    .detach_and_log_err(cx);
}

/// Marks `path` as an Icarus workspace before it opens, so its worktree is trusted the moment it is added,
/// also under the canonical path Zed's worktree reports when the folder is reached through a symlink.
pub(crate) fn add_icarus_workspace_path(path: &Path, cx: &mut App) {
    let canonical = std::fs::canonicalize(path).ok();
    let paths = &mut cx.default_global::<AppConnection>().workspace_paths;
    for path in std::iter::once(path.to_path_buf()).chain(canonical) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
}

/// Re-checks every open window after the list of Icarus workspaces changed.
pub(crate) fn set_icarus_workspace_paths(paths: Vec<PathBuf>, cx: &mut App) {
    cx.default_global::<AppConnection>().workspace_paths = Vec::new();
    for path in paths {
        add_icarus_workspace_path(&path, cx);
    }
    recheck_open_windows(cx);
}

/// Trusts newly known Icarus folders in every open window, closing a trust prompt they no longer need.
pub(crate) fn recheck_open_windows(cx: &mut App) {
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        window
            .update(cx, |multi_workspace, window, cx| {
                let workspaces: Vec<_> = multi_workspace.workspaces().cloned().collect();
                for workspace in workspaces {
                    workspace.update(cx, |workspace, cx| {
                        if trust_known(workspace, cx) {
                            return;
                        }
                        workspace.show_worktree_trust_security_modal(false, window, cx);
                    });
                }
            })
            .log_err();
    }
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
            return;
        };
        cx.subscribe_in(
            &trusted_worktrees,
            window,
            |workspace, _, event: &TrustedWorktreesEvent, window, cx| {
                if let TrustedWorktreesEvent::Restricted(worktree_store, _) = event {
                    let ours = workspace.project().read(cx).worktree_store().downgrade();
                    if *worktree_store == ours {
                        trust_icarus_worktrees(workspace, window, cx);
                    }
                }
            },
        )
        .detach();
        trust_icarus_worktrees(workspace, window, cx);
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_worktrees_inside_icarus_workspaces_are_trusted() {
        let icarus_paths = [PathBuf::from("/Users/me/.icarus/worktrees/feature")];
        let restricted = [
            (1, Path::new("/Users/me/.icarus/worktrees/feature")),
            (
                2,
                Path::new("/Users/me/.icarus/worktrees/feature/packages/web"),
            ),
            (3, Path::new("/Users/me/.icarus/worktrees/feature-two")),
            (4, Path::new("/Users/me/Downloads/untrusted")),
        ];
        assert_eq!(icarus_worktrees(&restricted, &icarus_paths), [1, 2]);
        assert!(icarus_worktrees(&restricted, &[]).is_empty());
    }
}
