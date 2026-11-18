//! Settings files read off the async threads, each at most once per change, with a bounded wait:
//! macOS can hold `open()` on a protected folder until the user answers its privacy prompt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use icarus_settings::Value;
use icarus_settings::layers::{self, Layer};

const READ_WAIT: Duration = Duration::from_secs(2);

/// A settings document and the file it came from; None when there is no file.
pub(crate) type Doc = Option<(PathBuf, Value)>;

#[derive(Debug, Clone)]
pub(crate) enum Failure {
    Invalid(String),
    Blocked(String),
}

type Stamp = Option<(SystemTime, u64)>;

#[derive(Default)]
struct Entry {
    /// The file and stamp the cached result was read from.
    read_from: Option<(PathBuf, Stamp)>,
    result: Option<Result<Doc, String>>,
    last_good: Option<Doc>,
    reading: Option<Arc<Reading>>,
    warned: bool,
}

struct Reading {
    started: Instant,
    done: Mutex<bool>,
    finished: Condvar,
}

type Key = (Layer, PathBuf);

static ENTRIES: LazyLock<Mutex<HashMap<Key, Entry>>> = LazyLock::new(Default::default);

fn entries() -> MutexGuard<'static, HashMap<Key, Entry>> {
    ENTRIES.lock().unwrap_or_else(|p| p.into_inner())
}

/// The layer's document, re-read only when its file changed. A read that hasn't finished within
/// [`READ_WAIT`] answers with the last good document and carries on in the background.
pub(crate) fn read(layer: Layer, path: &Path) -> Result<Doc, Failure> {
    let key = (layer, path.to_path_buf());
    let reading = {
        let mut entries = entries();
        let entry = entries.entry(key.clone()).or_default();
        match &entry.reading {
            Some(reading) => Arc::clone(reading),
            None => {
                let reading = Arc::new(Reading {
                    started: Instant::now(),
                    done: Mutex::new(false),
                    finished: Condvar::new(),
                });
                let known = entry
                    .result
                    .is_some()
                    .then(|| entry.read_from.clone())
                    .flatten();
                let (thread_key, thread_reading) = (key.clone(), Arc::clone(&reading));
                let spawned = std::thread::Builder::new()
                    .name("settings-read".into())
                    .spawn(move || refresh(thread_key, known, &thread_reading));
                if let Err(error) = spawned {
                    return Err(Failure::Blocked(format!(
                        "could not start reading {}: {error}",
                        path.display()
                    )));
                }
                entry.reading = Some(Arc::clone(&reading));
                reading
            }
        }
    };
    let done = wait(&reading);
    let mut entries = entries();
    let entry = entries.entry(key).or_default();
    if done && let Some(result) = &entry.result {
        return result.clone().map_err(Failure::Invalid);
    }
    if !entry.warned {
        entry.warned = true;
        tracing::warn!(
            "settings: reading {} is taking over {}s (macOS may be asking for permission); {}",
            path.display(),
            READ_WAIT.as_secs(),
            if entry.last_good.is_some() {
                "using its last good contents"
            } else {
                "it has no earlier contents to fall back on"
            }
        );
    }
    entry.last_good.clone().ok_or_else(|| {
        Failure::Blocked(format!(
            "reading {} did not finish within {}s; macOS may be waiting for permission to open it",
            path.display(),
            READ_WAIT.as_secs()
        ))
    })
}

/// Makes the next read of `path` read the file again, whatever its stamp says.
pub(crate) fn forget(path: &Path) {
    for ((_, key), entry) in entries().iter_mut() {
        if key == path {
            entry.read_from = None;
        }
    }
}

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn refresh(key: Key, known: Option<(PathBuf, Stamp)>, reading: &Reading) {
    let source = layers::layer_source(key.0, &key.1);
    let stamp = stamp(&source);
    let unchanged = stamp.is_some() && known.as_ref() == Some(&(source.clone(), stamp));
    let result = (!unchanged).then(|| {
        layers::read_document(&source)
            .map(|doc| doc.map(|doc| (source.clone(), doc)))
            .map_err(|error| format!("{error:?}"))
    });
    {
        let mut entries = entries();
        let entry = entries.entry(key.clone()).or_default();
        if let Some(result) = result {
            if let Ok(doc) = &result {
                entry.last_good = Some(doc.clone());
            }
            entry.result = Some(result);
            entry.read_from = Some((source, stamp));
        }
        if entry.warned {
            entry.warned = false;
            tracing::info!("settings: {} is readable again", key.1.display());
        }
        entry.reading = None;
    }
    *reading.done.lock().unwrap_or_else(|p| p.into_inner()) = true;
    reading.finished.notify_all();
}

fn wait(reading: &Reading) -> bool {
    let wait = || {
        let deadline = reading.started + READ_WAIT;
        let mut done = reading.done.lock().unwrap_or_else(|p| p.into_inner());
        while !*done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            done = reading
                .finished
                .wait_timeout(done, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    };
    // Let the runtime move this worker's other tasks elsewhere while it waits.
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(wait)
        }
        _ => wait(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_reread_only_after_it_changes() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("icarus.json");
        std::fs::write(&path, r#"{ "a": 1 }"#).expect("write");
        let first = read(Layer::Project, &path).expect("read").expect("doc");
        assert_eq!(first.1.get("a").and_then(number), Some(1));
        std::fs::write(&path, r#"{ "a": 22 }"#).expect("write");
        let second = read(Layer::Project, &path).expect("read").expect("doc");
        assert_eq!(second.1.get("a").and_then(number), Some(22));
        std::fs::remove_file(&path).expect("remove");
        assert!(read(Layer::Project, &path).expect("read").is_none());
    }

    #[test]
    fn a_file_that_blocks_falls_back_to_its_last_good_contents_then_recovers() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("icarus.json");
        let never_read = dir.path().join("icarus.local.json");
        make_fifo(&never_read);
        let started = Instant::now();
        let error = read(Layer::ProjectLocal, &never_read).expect_err("blocks");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(
            matches!(&error, Failure::Blocked(m) if m.contains("did not finish")),
            "{error:?}"
        );
        let started = Instant::now();
        assert!(read(Layer::ProjectLocal, &never_read).is_err());
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a second caller waits again"
        );

        std::fs::write(&path, r#"{ "a": 1 }"#).expect("write");
        assert!(read(Layer::Project, &path).expect("read").is_some());
        std::fs::remove_file(&path).expect("remove");
        make_fifo(&path);
        let doc = read(Layer::Project, &path)
            .expect("last good")
            .expect("doc");
        assert_eq!(doc.1.get("a").and_then(number), Some(1));

        std::fs::write(&path, r#"{ "a": 2 }"#).expect("unblock the reader");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some((_, doc))) = read(Layer::Project, &path)
                && doc.get("a").and_then(number) == Some(2)
            {
                break;
            }
            assert!(Instant::now() < deadline, "never recovered");
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::write(&never_read, "{}");
    }

    fn number(value: &Value) -> Option<i64> {
        value.as_number()?.as_i64()
    }

    fn make_fifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo");
        assert!(status.success());
    }
}
