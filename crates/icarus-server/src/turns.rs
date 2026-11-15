use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use icarus_git::turns::{self, TurnMeta};

use crate::agent::{AgentInfo, TurnHook};

struct Pending {
    cwd: PathBuf,
    start: String,
    started_at: u64,
    prompt: String,
}

#[derive(Default)]
pub struct TurnRecorder {
    pending: Mutex<HashMap<String, Pending>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl TurnRecorder {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl TurnHook for TurnRecorder {
    fn turn_started(&self, agent: &AgentInfo, prompt: &str) {
        // A steering message joins the running turn instead of starting a new snapshot.
        if self.pending().contains_key(&agent.id) || !turns::is_worktree(&agent.cwd) {
            return;
        }
        match turns::start_turn(&agent.cwd) {
            Ok(start) => {
                self.pending().insert(
                    agent.id.clone(),
                    Pending {
                        cwd: agent.cwd.clone(),
                        start,
                        started_at: now_ms(),
                        prompt: prompt.to_string(),
                    },
                );
            }
            Err(error) => tracing::warn!("turn start snapshot for {}: {error:#}", agent.id),
        }
    }

    fn turn_ended(&self, agent: &AgentInfo) {
        let Some(pending) = self.pending().remove(&agent.id) else {
            return;
        };
        let meta = TurnMeta {
            prompt: pending.prompt,
            agent_id: agent.id.clone(),
            provider: Some(agent.provider.clone()),
            started_at: pending.started_at,
            completed_at: now_ms(),
        };
        if let Err(error) = turns::finish_turn(&pending.cwd, &pending.start, &meta) {
            tracing::warn!("turn end snapshot for {}: {error:#}", agent.id);
        }
    }
}
