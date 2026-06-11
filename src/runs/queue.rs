use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use tokio::sync::Notify;

use crate::protocol::ids::SessionId;

/// Global FIFO of sessions awaiting an agent run. A session appears at most once;
/// popping is the single-flight point — one consumer means one run at a time.
#[derive(Default)]
pub struct RunQueue {
    state: Mutex<State>,
    notify: Notify,
}

#[derive(Default)]
struct State {
    order: VecDeque<SessionId>,
    queued: HashSet<SessionId>,
}

impl RunQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns false when the session was already queued.
    pub fn enqueue(&self, session: SessionId) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.queued.insert(session.clone()) {
            return false;
        }
        state.order.push_back(session);
        drop(state);
        self.notify.notify_one();
        true
    }

    /// Waits until a session is available, then claims it.
    pub async fn next(&self) -> SessionId {
        loop {
            let notified = self.notify.notified();
            if let Some(session) = self.pop() {
                return session;
            }
            notified.await;
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<SessionId> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.order.iter().cloned().collect()
    }

    fn pop(&self) -> Option<SessionId> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = state.order.pop_front()?;
        state.queued.remove(&session);
        Some(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sid(raw: &str) -> SessionId {
        SessionId::new(raw)
    }

    #[test]
    fn enqueue_deduplicates_until_popped() {
        let queue = RunQueue::new();
        assert!(queue.enqueue(sid("a")));
        assert!(!queue.enqueue(sid("a")));
        assert!(queue.enqueue(sid("b")));
        assert_eq!(queue.snapshot(), vec![sid("a"), sid("b")]);
        assert_eq!(queue.pop(), Some(sid("a")));
        assert!(queue.enqueue(sid("a")));
        assert_eq!(queue.snapshot(), vec![sid("b"), sid("a")]);
    }

    #[tokio::test]
    async fn next_returns_in_fifo_order() {
        let queue = RunQueue::new();
        queue.enqueue(sid("a"));
        queue.enqueue(sid("b"));
        assert_eq!(queue.next().await, sid("a"));
        assert_eq!(queue.next().await, sid("b"));
    }

    #[tokio::test]
    async fn next_wakes_up_when_work_arrives() {
        let queue = std::sync::Arc::new(RunQueue::new());
        let waiter = {
            let queue = queue.clone();
            tokio::spawn(async move { queue.next().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        queue.enqueue(sid("late"));
        let got = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("must not time out")
            .expect("task must not panic");
        assert_eq!(got, sid("late"));
    }
}
