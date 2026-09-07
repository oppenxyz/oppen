//! ES19 ownership of blocking local reads, independent of IPC observers.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadKind {
    Operator,
    Pilot,
    Keychain,
}

impl ReadKind {
    fn index(self) -> usize {
        match self {
            Self::Operator => 0,
            Self::Pilot => 1,
            Self::Keychain => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadError {
    Busy,
    Stopping,
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "A local read of this kind is still in progress.",
            Self::Stopping => "Local reads are stopping; new reads are not accepted.",
        })
    }
}

impl std::error::Error for ReadError {}

#[derive(Debug, Default)]
struct State {
    stopping: bool,
    active: [bool; 3],
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    completed: Notify,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        // No user callback or I/O runs under this mutex; its only mutations
        // are admission and completion flags.
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// One owner per runtime context. Stopping is permanent; a replacement context
/// creates a new owner only after this one's admitted closures have drained.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalReads {
    inner: Arc<Inner>,
}

impl LocalReads {
    pub(crate) fn spawn<T: Send + 'static>(
        &self,
        kind: ReadKind,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> Result<tauri::async_runtime::JoinHandle<T>, ReadError> {
        let mut state = self.inner.state();
        if state.stopping {
            return Err(ReadError::Stopping);
        }
        if state.active[kind.index()] {
            return Err(ReadError::Busy);
        }
        state.active[kind.index()] = true;
        let permit = ReadPermit {
            inner: self.inner.clone(),
            kind,
        };
        drop(state);
        Ok(tauri::async_runtime::spawn_blocking(move || {
            // Ownership starts at admission, including time queued in the
            // blocking pool, and survives a dropped IPC/join observer.
            let _permit = permit;
            read()
        }))
    }

    pub(crate) fn begin_stop(&self) {
        self.inner.state().stopping = true;
    }

    pub(crate) async fn drain(&self) {
        self.begin_stop();
        loop {
            let completed = self.inner.completed.notified();
            tokio::pin!(completed);
            // Register before checking the predicate. notify_waiters wakes all
            // registered observers; the flags retain evidence for later ones.
            completed.as_mut().enable();
            if !self.inner.state().active.iter().any(|active| *active) {
                return;
            }
            completed.await;
        }
    }
}

struct ReadPermit {
    inner: Arc<Inner>,
    kind: ReadKind,
}

impl Drop for ReadPermit {
    fn drop(&mut self) {
        self.inner.state().active[self.kind.index()] = false;
        self.inner.completed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::mpsc;
    use std::task::Poll;
    use std::time::Duration;
    use tokio::sync::oneshot;

    const DEADLINE: Duration = Duration::from_secs(5);

    async fn blocked(
        reads: &LocalReads,
        kind: ReadKind,
    ) -> (tauri::async_runtime::JoinHandle<()>, mpsc::Sender<()>) {
        let (started, observing) = oneshot::channel();
        let (release, waiting) = mpsc::channel();
        let task = reads
            .spawn(kind, move || {
                started.send(()).expect("start observer");
                waiting.recv().expect("release blocked read");
            })
            .expect("admit read");
        tokio::time::timeout(DEADLINE, observing)
            .await
            .expect("read start deadline")
            .expect("read started");
        (task, release)
    }

    #[tokio::test]
    async fn dropped_join_observer_does_not_release_its_slot() {
        let reads = LocalReads::default();
        let (task, release) = blocked(&reads, ReadKind::Operator).await;
        drop(task);
        let cloned = reads.clone();
        assert!(matches!(
            cloned.spawn(ReadKind::Operator, || panic!("overlap")),
            Err(ReadError::Busy)
        ));
        release.send(()).unwrap();
        tokio::time::timeout(DEADLINE, async {
            loop {
                match cloned.spawn(ReadKind::Operator, || 7) {
                    Ok(task) => {
                        assert_eq!(task.await.unwrap(), 7);
                        break;
                    }
                    Err(ReadError::Busy) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected refusal: {error}"),
                }
            }
        })
        .await
        .expect("slot released after actual completion");
        reads.drain().await;
    }

    #[tokio::test]
    async fn operator_pilot_and_keychain_have_independent_slots() {
        let reads = LocalReads::default();
        let (operator, release_operator) = blocked(&reads, ReadKind::Operator).await;
        let (pilot, release_pilot) = blocked(&reads, ReadKind::Pilot).await;
        let (keychain, release_keychain) = blocked(&reads, ReadKind::Keychain).await;
        for kind in [ReadKind::Operator, ReadKind::Pilot, ReadKind::Keychain] {
            assert!(matches!(reads.spawn(kind, || ()), Err(ReadError::Busy)));
        }
        release_pilot.send(()).unwrap();
        pilot.await.unwrap();
        assert_eq!(
            reads.spawn(ReadKind::Pilot, || 3).unwrap().await.unwrap(),
            3
        );
        assert!(matches!(
            reads.spawn(ReadKind::Operator, || ()),
            Err(ReadError::Busy)
        ));
        assert!(matches!(
            reads.spawn(ReadKind::Keychain, || ()),
            Err(ReadError::Busy)
        ));
        release_keychain.send(()).unwrap();
        keychain.await.unwrap();
        release_operator.send(()).unwrap();
        operator.await.unwrap();
        reads.drain().await;
    }

    #[tokio::test]
    async fn blocked_keychain_read_cannot_starve_pilot() {
        let reads = LocalReads::default();
        let (keychain, release_keychain) = blocked(&reads, ReadKind::Keychain).await;
        let pilot = reads
            .spawn(ReadKind::Pilot, || 11)
            .expect("independent pilot slot");
        assert_eq!(
            tokio::time::timeout(DEADLINE, pilot)
                .await
                .expect("pilot completed while keychain blocked")
                .unwrap(),
            11
        );
        assert!(matches!(
            reads.spawn(ReadKind::Keychain, || ()),
            Err(ReadError::Busy)
        ));
        release_keychain.send(()).unwrap();
        keychain.await.unwrap();
        reads.drain().await;
    }

    #[tokio::test]
    async fn drain_waits_for_all_three_kinds() {
        let reads = LocalReads::default();
        let (operator, release_operator) = blocked(&reads, ReadKind::Operator).await;
        let (pilot, release_pilot) = blocked(&reads, ReadKind::Pilot).await;
        let (keychain, release_keychain) = blocked(&reads, ReadKind::Keychain).await;
        let mut drain = Box::pin(reads.drain());
        std::future::poll_fn(|context| {
            assert!(drain.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        release_operator.send(()).unwrap();
        operator.await.unwrap();
        std::future::poll_fn(|context| {
            assert!(drain.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        release_pilot.send(()).unwrap();
        pilot.await.unwrap();
        std::future::poll_fn(|context| {
            assert!(drain.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        release_keychain.send(()).unwrap();
        tokio::time::timeout(DEADLINE, drain)
            .await
            .expect("all three kinds completed");
        keychain.await.unwrap();
    }

    #[tokio::test]
    async fn begin_stop_closes_all_three_slots_permanently() {
        let reads = LocalReads::default();
        let (task, release) = blocked(&reads, ReadKind::Pilot).await;
        reads.begin_stop();
        reads.begin_stop();
        for kind in [ReadKind::Operator, ReadKind::Pilot, ReadKind::Keychain] {
            assert!(matches!(
                reads.clone().spawn(kind, || ()),
                Err(ReadError::Stopping)
            ));
        }
        release.send(()).unwrap();
        task.await.unwrap();
        reads.drain().await;
        assert!(matches!(
            reads.spawn(ReadKind::Pilot, || ()),
            Err(ReadError::Stopping)
        ));
    }

    #[tokio::test]
    async fn dropped_drain_observer_does_not_lose_completion() {
        let reads = LocalReads::default();
        let (task, release) = blocked(&reads, ReadKind::Operator).await;
        drop(task);
        let mut first = Box::pin(reads.drain());
        std::future::poll_fn(|context| {
            assert!(first.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(first);
        assert!(matches!(
            reads.spawn(ReadKind::Pilot, || ()),
            Err(ReadError::Stopping)
        ));
        let mut retry = Box::pin(reads.drain());
        std::future::poll_fn(|context| {
            assert!(retry.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        tokio::time::timeout(DEADLINE, retry)
            .await
            .expect("retry observes completion");
        tokio::time::timeout(DEADLINE, reads.drain())
            .await
            .expect("late observer sees completed state");
    }

    #[tokio::test]
    async fn panicking_read_releases_all_drain_waiters() {
        let reads = LocalReads::default();
        let (started, observing) = oneshot::channel();
        let (release, waiting) = mpsc::channel();
        let task = reads
            .spawn(ReadKind::Pilot, move || {
                started.send(()).unwrap();
                waiting.recv().unwrap();
                panic!("injected local read failure");
            })
            .unwrap();
        tokio::time::timeout(DEADLINE, observing)
            .await
            .unwrap()
            .unwrap();
        let other = reads.clone();
        let mut first = Box::pin(reads.drain());
        let mut second = Box::pin(other.drain());
        std::future::poll_fn(|context| {
            assert!(first.as_mut().poll(context).is_pending());
            assert!(second.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        tokio::time::timeout(DEADLINE, async {
            tokio::join!(first, second);
            assert!(task.await.is_err());
        })
        .await
        .expect("panic releases every waiter");
    }
}
