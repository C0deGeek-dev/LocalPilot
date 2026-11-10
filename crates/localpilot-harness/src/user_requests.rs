//! Shared, session-scoped views a context hook may read: the user's recent
//! instructions and the live trust state.
//!
//! Both are cheap to clone and shared between the runtime, which updates them,
//! and hooks, which only read. The user's instructions are rebuilt from the
//! session log on resume, so a hook sees the task in progress after a restart
//! without a separate record.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The most recent user instructions kept.
const MAX_REQUESTS: usize = 16;

/// The user's recent instructions in this session, oldest first.
#[derive(Debug, Clone, Default)]
pub struct UserRequests {
    requests: Arc<Mutex<VecDeque<String>>>,
}

impl UserRequests {
    /// Record a new instruction.
    pub fn push(&self, request: &str) {
        if let Ok(mut requests) = self.requests.lock() {
            if requests.len() == MAX_REQUESTS {
                requests.pop_front();
            }
            requests.push_back(request.to_string());
        }
    }

    /// Replace the instructions, keeping the most recent ones.
    pub fn replace(&self, requests: impl IntoIterator<Item = String>) {
        if let Ok(mut held) = self.requests.lock() {
            held.clear();
            for request in requests {
                if held.len() == MAX_REQUESTS {
                    held.pop_front();
                }
                held.push_back(request);
            }
        }
    }

    /// The instructions, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.requests
            .lock()
            .map(|requests| requests.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// The session's live trust state.
#[derive(Debug, Clone, Default)]
pub struct TrustFlag {
    trusted: Arc<AtomicBool>,
}

impl TrustFlag {
    /// A flag starting at `trusted`.
    #[must_use]
    pub fn new(trusted: bool) -> Self {
        Self {
            trusted: Arc::new(AtomicBool::new(trusted)),
        }
    }

    /// Whether the workspace is trusted now.
    #[must_use]
    pub fn get(&self) -> bool {
        self.trusted.load(Ordering::Relaxed)
    }

    pub(crate) fn set(&self, trusted: bool) {
        self.trusted.store(trusted, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_bounded_and_replaced_in_order() {
        let requests = UserRequests::default();
        for n in 0..(MAX_REQUESTS + 2) {
            requests.push(&n.to_string());
        }
        let held = requests.snapshot();
        assert_eq!(held.len(), MAX_REQUESTS);
        assert_eq!(held[0], "2");
        requests.replace(["a".to_string(), "b".to_string()]);
        assert_eq!(requests.snapshot(), ["a", "b"]);
    }
}
