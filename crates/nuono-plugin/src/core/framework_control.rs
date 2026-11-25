use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameworkControlSignal {
    Shutdown { reason: String },
    Restart { reason: String },
}

impl FrameworkControlSignal {
    pub fn is_restart(&self) -> bool {
        matches!(self, Self::Restart { .. })
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Shutdown { reason } | Self::Restart { reason } => reason,
        }
    }
}

#[derive(Clone)]
pub struct FrameworkControl {
    inner: Arc<FrameworkControlInner>,
}

impl std::fmt::Debug for FrameworkControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameworkControl")
            .field("last_signal", &self.last_signal())
            .finish_non_exhaustive()
    }
}

struct FrameworkControlInner {
    tx: broadcast::Sender<FrameworkControlSignal>,
    last_signal: Mutex<Option<FrameworkControlSignal>>,
}

impl FrameworkControl {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(8);
        Self {
            inner: Arc::new(FrameworkControlInner {
                tx,
                last_signal: Mutex::new(None),
            }),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<FrameworkControlSignal> {
        self.inner.tx.subscribe()
    }

    pub fn request_shutdown(&self, reason: impl Into<String>) {
        self.send(FrameworkControlSignal::Shutdown {
            reason: reason.into(),
        });
    }

    pub fn request_restart(&self, reason: impl Into<String>) {
        self.send(FrameworkControlSignal::Restart {
            reason: reason.into(),
        });
    }

    pub fn last_signal(&self) -> Option<FrameworkControlSignal> {
        self.inner
            .last_signal
            .lock()
            .ok()
            .and_then(|signal| signal.clone())
    }

    fn send(&self, signal: FrameworkControlSignal) {
        if let Ok(mut last) = self.inner.last_signal.lock() {
            *last = Some(signal.clone());
        }
        let _ = self.inner.tx.send(signal);
    }
}

impl Default for FrameworkControl {
    fn default() -> Self {
        Self::new()
    }
}
