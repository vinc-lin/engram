use crate::error::Result;
use crate::store::{Job, Store};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What the cold worker does with a claimed job. Plan 4a ships `NoopProcessor`;
/// Plan 4b swaps in the tree processor.
pub trait JobProcessor: Send + Sync {
    fn process(&self, store: &Store, job: &Job) -> Result<()>;
}

/// Marks jobs done without work — proves the queue end-to-end before the tree exists.
pub struct NoopProcessor;
impl JobProcessor for NoopProcessor {
    fn process(&self, _store: &Store, _job: &Job) -> Result<()> {
        Ok(())
    }
}

/// Claim one pending job and process it. Returns `Ok(true)` if a job was handled,
/// `Ok(false)` if the queue was empty.
pub fn worker_tick(store: &Store, processor: &dyn JobProcessor, max_attempts: i64) -> Result<bool> {
    let job = match store.claim_job()? {
        Some(j) => j,
        None => return Ok(false),
    };
    match processor.process(store, &job) {
        Ok(()) => store.complete_job(&job.job_id)?,
        Err(e) => store.fail_or_retry_job(&job.job_id, &e.to_string(), max_attempts)?,
    }
    Ok(true)
}

/// Spawn `workers` threads draining the queue until `stop` is set. Idle threads sleep `poll_ms`.
pub fn spawn_workers(
    store: Store,
    processor: Arc<dyn JobProcessor>,
    workers: usize,
    poll_ms: u64,
    max_attempts: i64,
    stop: Arc<AtomicBool>,
) -> Vec<std::thread::JoinHandle<()>> {
    (0..workers)
        .map(|_| {
            let store = store.clone();
            let processor = processor.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match worker_tick(&store, processor.as_ref(), max_attempts) {
                        Ok(true) => {} // got work — loop immediately for more
                        Ok(false) => std::thread::sleep(std::time::Duration::from_millis(poll_ms)),
                        Err(e) => {
                            tracing::warn!("worker_tick error: {e}");
                            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
                        }
                    }
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Job, Store};

    fn temp() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        (store, dir)
    }

    struct Failing;
    impl JobProcessor for Failing {
        fn process(&self, _store: &Store, _job: &Job) -> crate::error::Result<()> {
            Err(crate::error::Error::Llm("nope".into()))
        }
    }

    #[test]
    fn tick_processes_pending_with_noop() {
        let (store, _d) = temp();
        store.enqueue_job("alice", "d").unwrap();
        assert!(worker_tick(&store, &NoopProcessor, 5).unwrap()); // handled one
        assert_eq!(store.job("alice:d").unwrap().unwrap().0, "done");
        assert!(!worker_tick(&store, &NoopProcessor, 5).unwrap()); // nothing left
    }

    #[test]
    fn tick_retries_then_fails() {
        let (store, _d) = temp();
        store.enqueue_job("alice", "d").unwrap();
        worker_tick(&store, &Failing, 2).unwrap(); // attempts 1 → pending
        assert_eq!(
            store.job("alice:d").unwrap().unwrap(),
            ("pending".into(), 1)
        );
        worker_tick(&store, &Failing, 2).unwrap(); // attempts 2 → failed
        assert_eq!(store.job("alice:d").unwrap().unwrap(), ("failed".into(), 2));
    }

    #[test]
    fn workers_drain_in_background() {
        let (store, _d) = temp();
        store.enqueue_job("alice", "d").unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handles = spawn_workers(
            store.clone(),
            std::sync::Arc::new(NoopProcessor),
            1,
            20,
            5,
            stop.clone(),
        );
        // poll until done (≤2s)
        let mut done = false;
        for _ in 0..100 {
            if store
                .job("alice:d")
                .unwrap()
                .map(|(s, _)| s == "done")
                .unwrap_or(false)
            {
                done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
        assert!(done);
    }
}
