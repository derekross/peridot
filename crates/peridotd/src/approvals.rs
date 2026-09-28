//! Dangerous requests from the `peridot` command wait for a yes in the
//! panel: the command can't prove a person is at the keyboard, the panel
//! can. The daemon announces a pending approval to the panel, blocks the
//! request, and lets it through or refuses it when the panel answers or
//! two minutes pass. Desktop notifications only point at the panel; their
//! buttons are never an answer, because any process of yours could press
//! them.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::oneshot;

/// How long a request waits for the panel.
pub const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub id: u64,
    pub method: String,
    /// One line: what it would do.
    pub summary: String,
    /// Who asked.
    pub from: &'static str,
    pub expires_at: u64,
}

struct Waiting {
    pending: Pending,
    tx: oneshot::Sender<bool>,
}

#[derive(Default)]
pub struct Approvals {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    waiting: HashMap<u64, Waiting>,
}

impl Approvals {
    /// Register a request; returns its description (to announce) and the
    /// receiver to wait on.
    pub fn open(
        &self,
        method: &str,
        summary: String,
        from: &'static str,
        now: u64,
    ) -> (Pending, oneshot::Receiver<bool>) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.next_id += 1;
        let pending = Pending {
            id: g.next_id,
            method: method.to_string(),
            summary,
            from,
            expires_at: now + APPROVAL_TIMEOUT.as_secs(),
        };
        let (tx, rx) = oneshot::channel();
        g.waiting.insert(
            pending.id,
            Waiting {
                pending: pending.clone(),
                tx,
            },
        );
        (pending, rx)
    }

    /// The panel answered. False if nothing was waiting under that id.
    pub fn answer(&self, id: u64, ok: bool) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match g.waiting.remove(&id) {
            Some(w) => {
                let _ = w.tx.send(ok);
                true
            }
            None => false,
        }
    }

    /// Drop a request that timed out or whose caller went away.
    pub fn forget(&self, id: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.waiting.remove(&id);
    }

    /// What the panel should show.
    pub fn list(&self) -> Vec<Pending> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut v: Vec<Pending> = g.waiting.values().map(|w| w.pending.clone()).collect();
        v.sort_by_key(|p| p.id);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approvals_are_answered_once_and_listed_while_open() {
        let a = Approvals::default();
        let (p, rx) = a.open(
            "setup.leave",
            "stop syncing".into(),
            "the peridot command",
            10,
        );
        assert_eq!(a.list().len(), 1);
        assert_eq!(p.expires_at, 130);
        assert!(a.answer(p.id, true));
        assert_eq!(rx.await, Ok(true));
        assert!(!a.answer(p.id, true), "gone once answered");
        assert!(a.list().is_empty());
        let (p2, rx2) = a.open("x", "y".into(), "the peridot command", 10);
        a.forget(p2.id);
        assert!(rx2.await.is_err(), "a forgotten request is refused");
    }
}
