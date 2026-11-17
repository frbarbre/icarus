use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use icarus_protocol::RpcError;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::server::rpc;

type Reply = Result<Value, RpcError>;
type Waiting = Arc<Mutex<HashMap<String, Vec<oneshot::Sender<Reply>>>>>;

/// Identical reads that arrive while one runs share its answer instead of running git again:
/// every window refreshes the same workspace, and each refresh is a `git status`.
#[derive(Default)]
pub(crate) struct Coalesced {
    waiting: Waiting,
}

/// Forgets the run when its task ends, even by panicking, so later requests start a new one.
struct Running {
    waiting: Waiting,
    key: String,
}

impl Running {
    fn take(&self) -> Vec<oneshot::Sender<Reply>> {
        self.waiting
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.key)
            .unwrap_or_default()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.take();
    }
}

impl Coalesced {
    pub(crate) async fn run<F>(&self, key: String, work: impl FnOnce() -> F) -> Reply
    where
        F: Future<Output = Reply> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let leads = {
            let mut waiting = self
                .waiting
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match waiting.get_mut(&key) {
                Some(senders) => {
                    senders.push(sender);
                    false
                }
                None => {
                    waiting.insert(key.clone(), vec![sender]);
                    true
                }
            }
        };
        if leads {
            let running = Running {
                waiting: self.waiting.clone(),
                key,
            };
            let work = work();
            // Spawned so a caller that disconnects doesn't cancel the answer the others wait for.
            tokio::spawn(async move {
                let reply = work.await;
                for sender in running.take() {
                    sender.send(reply.clone()).ok();
                }
            });
        }
        receiver.await.unwrap_or_else(|_| {
            Err(rpc(
                "internal",
                "the shared request ended without an answer",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn concurrent_identical_requests_run_once_and_later_ones_run_again() {
        let coalesced = Arc::new(Coalesced::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let call = |key: &str| {
            let coalesced = coalesced.clone();
            let runs = runs.clone();
            let key = key.to_owned();
            async move {
                coalesced
                    .run(key, || async move {
                        let run = runs.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(json!(run))
                    })
                    .await
            }
        };
        let (a, b, c, other) = tokio::join!(call("a"), call("a"), call("a"), call("b"));
        assert_eq!(runs.load(Ordering::SeqCst), 2, "one run per key");
        assert_eq!((a.clone(), b, c), (a.clone(), a.clone(), a));
        assert!(other.is_ok());
        call("a").await.expect("a later request");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            3,
            "a finished run is not reused"
        );
    }

    #[tokio::test]
    async fn a_failed_run_answers_every_waiter_with_its_error() {
        let coalesced = Arc::new(Coalesced::default());
        let call = || {
            let coalesced = coalesced.clone();
            async move {
                coalesced
                    .run("k".into(), || async {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Err(rpc("git_failed", "boom"))
                    })
                    .await
            }
        };
        let (a, b) = tokio::join!(call(), call());
        assert_eq!(a.expect_err("a").code, "git_failed");
        assert_eq!(b.expect_err("b").code, "git_failed");
    }
}
