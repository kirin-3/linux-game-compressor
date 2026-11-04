//! Coordinates estimator samples with a compression queue.
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

struct State {
    epoch: u64,
    transitioning: bool,
    next: u64,
    active: HashMap<u64, (PathBuf, CancellationToken)>,
    held: Option<OwnedSemaphorePermit>,
    handoff_abort: Option<tokio::task::AbortHandle>,
}
pub struct SampleLimiter {
    permits: Arc<Semaphore>,
    count: u32,
    state: Mutex<State>,
    notify: Notify,
}
pub struct SamplePermit {
    owner: Arc<SampleLimiter>,
    id: u64,
    _permit: OwnedSemaphorePermit,
    pub cancel: CancellationToken,
}
impl Drop for SamplePermit {
    fn drop(&mut self) {
        self.owner.state.lock().unwrap().active.remove(&self.id);
    }
}
impl SampleLimiter {
    pub fn new(count: usize) -> Arc<Self> {
        let count = count.max(1).min(u32::MAX as usize) as u32;
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(count as usize)),
            count,
            state: Mutex::new(State {
                epoch: 0,
                transitioning: false,
                next: 0,
                active: HashMap::new(),
                held: None,
                handoff_abort: None,
            }),
            notify: Notify::new(),
        })
    }
    pub async fn acquire(
        self: &Arc<Self>,
        path: PathBuf,
        cancel: &CancellationToken,
    ) -> io::Result<SamplePermit> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().unwrap().transitioning {
                tokio::select! { _ = notified => {}, _ = cancel.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")) }
                continue;
            }
            let permit = tokio::select! { p = self.permits.clone().acquire_owned() => p.map_err(io::Error::other)?, _ = cancel.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")) };
            let mut state = self.state.lock().unwrap();
            if state.transitioning {
                drop(state);
                drop(permit);
                continue;
            }
            let token = cancel.child_token();
            let id = state.next;
            state.next += 1;
            state.active.insert(id, (path.clone(), token.clone()));
            return Ok(SamplePermit {
                owner: self.clone(),
                id,
                _permit: permit,
                cancel: token,
            });
        }
    }
    /// Cancel active encodes and wait for them to relinquish all permits. On timeout,
    /// return stuck paths and keep new samples gated until the handoff completes.
    pub async fn prepare_queue(self: &Arc<Self>, timeout: Duration) -> Vec<PathBuf> {
        let epoch = {
            let mut state = self.state.lock().unwrap();
            state.epoch += 1;
            state.transitioning = true;
            for (_, token) in state.active.values() {
                token.cancel();
            }
            state.epoch
        };
        let limiter = self.clone();
        let task = tokio::spawn(async move {
            let Ok(mut all) = limiter
                .permits
                .clone()
                .acquire_many_owned(limiter.count)
                .await
            else {
                return;
            };
            let mut state = limiter.state.lock().unwrap();
            if state.epoch != epoch {
                return;
            }
            // Release one permit for the running queue's estimates.
            drop(all.split(1));
            state.held = Some(all);
            state.transitioning = false;
            drop(state);
            limiter.notify.notify_waiters();
        });
        let mut task = task;
        {
            let mut state = self.state.lock().unwrap();
            if state.epoch == epoch {
                state.handoff_abort = Some(task.abort_handle());
            } else {
                task.abort();
            }
        }
        if tokio::time::timeout(timeout, &mut task).await.is_ok() {
            Vec::new()
        } else {
            self.state
                .lock()
                .unwrap()
                .active
                .values()
                .map(|(path, _)| path.clone())
                .collect()
        }
    }
    pub fn restore(&self) {
        let mut state = self.state.lock().unwrap();
        state.epoch += 1;
        state.transitioning = false;
        if let Some(task) = state.handoff_abort.take() {
            task.abort();
        }
        state.held.take();
        drop(state);
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn handoff_kills_an_encoder_instead_of_waiting_for_it() {
        let limiter = SampleLimiter::new(2);
        let lifetime = CancellationToken::new();
        let permit = limiter
            .acquire("video.webm".into(), &lifetime)
            .await
            .unwrap();
        let token = permit.cancel.clone();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let arg = marker.clone();
        let process = tokio::spawn(async move {
            let result = compressor_core::process::run_process(
                std::ffi::OsStr::new("/bin/sh"),
                [
                    std::ffi::OsStr::new("-c"),
                    std::ffi::OsStr::new("echo ready > \"$1\"; sleep 30"),
                    std::ffi::OsStr::new("encoder"),
                    arg.as_os_str(),
                ],
                &token,
                &token,
            )
            .await
            .unwrap();
            drop(permit);
            result.cancelled
        });
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(marker.exists());
        let start = std::time::Instant::now();
        assert!(limiter
            .prepare_queue(Duration::from_secs(5))
            .await
            .is_empty());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(process.await.unwrap());
        limiter.restore();
    }
    #[tokio::test]
    async fn start_drains_samples_and_finish_restores_parallelism() {
        let limiter = SampleLimiter::new(3);
        let lifetime = CancellationToken::new();
        let first = limiter.acquire("first".into(), &lifetime).await.unwrap();
        let second = limiter.acquire("second".into(), &lifetime).await.unwrap();
        let handoff = {
            let l = limiter.clone();
            tokio::spawn(async move { l.prepare_queue(Duration::from_secs(2)).await })
        };
        first.cancel.cancelled().await;
        second.cancel.cancelled().await;
        assert!(!lifetime.is_cancelled());
        drop(first);
        drop(second);
        assert!(handoff.await.unwrap().is_empty());
        let one = limiter.acquire("one".into(), &lifetime).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            limiter.acquire("two".into(), &lifetime)
        )
        .await
        .is_err());
        drop(one);
        limiter.restore();
        let a = limiter.acquire("a".into(), &lifetime).await.unwrap();
        let b = limiter.acquire("b".into(), &lifetime).await.unwrap();
        let c = limiter.acquire("c".into(), &lifetime).await.unwrap();
        drop((a, b, c));
    }
    #[tokio::test]
    async fn timeout_reports_path_and_keeps_new_samples_gated() {
        let limiter = SampleLimiter::new(2);
        let lifetime = CancellationToken::new();
        let stuck = limiter.acquire("stuck".into(), &lifetime).await.unwrap();
        let paths = limiter.prepare_queue(Duration::from_millis(20)).await;
        assert_eq!(paths, vec![PathBuf::from("stuck")]);
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            limiter.acquire("new".into(), &lifetime)
        )
        .await
        .is_err());
        drop(stuck);
        let _one = tokio::time::timeout(
            Duration::from_secs(1),
            limiter.acquire("new".into(), &lifetime),
        )
        .await
        .unwrap()
        .unwrap();
        limiter.restore();
    }
    #[tokio::test]
    async fn restore_after_timeout_releases_waiters_even_if_sample_is_still_stuck() {
        let limiter = SampleLimiter::new(2);
        let lifetime = CancellationToken::new();
        let stuck = limiter.acquire("stuck".into(), &lifetime).await.unwrap();
        assert_eq!(
            limiter.prepare_queue(Duration::from_millis(20)).await,
            vec![PathBuf::from("stuck")]
        );
        limiter.restore();
        let next = tokio::time::timeout(
            Duration::from_secs(1),
            limiter.acquire("next".into(), &lifetime),
        )
        .await
        .unwrap()
        .unwrap();
        drop((stuck, next));
    }
    #[tokio::test]
    async fn one_permit_handoff_still_waits_for_the_active_sample() {
        let limiter = SampleLimiter::new(1);
        let lifetime = CancellationToken::new();
        let active = limiter.acquire("only".into(), &lifetime).await.unwrap();
        let transition = {
            let limiter = limiter.clone();
            tokio::spawn(async move { limiter.prepare_queue(Duration::from_secs(1)).await })
        };
        active.cancel.cancelled().await;
        drop(active);
        assert!(transition.await.unwrap().is_empty());
        let next = limiter.acquire("next".into(), &lifetime).await.unwrap();
        drop(next);
        limiter.restore();
    }
}
