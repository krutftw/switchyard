//! How long-lived handlers learn that the server is shutting down.
//!
//! [`crate::BoundServer::serve`] puts a [`Lifecycle`] into the extensions of
//! every request. HTTP requests are drained by hyper itself and never look
//! at it; a WebSocket session — which hyper stops tracking at the upgrade —
//! holds on to it for as long as it lives, watches the two tokens, and by
//! being alive tells the server that there is still something to wait for.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A handle on the server's shutdown sequence, cloned into each request.
#[derive(Clone, Debug)]
pub(crate) struct Lifecycle {
    /// Cancelled when shutdown is requested: finish what is in progress,
    /// start nothing new, say goodbye.
    pub(crate) shutdown: CancellationToken,
    /// Cancelled when the grace period is over: stop now.
    pub(crate) kill: CancellationToken,
    /// Never sent on. The receiver reports "closed" once every clone of
    /// this sender — every request and every session — is gone.
    _alive: mpsc::Sender<()>,
}

/// The server's end of the sequence.
#[derive(Debug)]
pub(crate) struct LifecycleOwner {
    handle: Option<Lifecycle>,
    shutdown: CancellationToken,
    kill: CancellationToken,
    alive: mpsc::Receiver<()>,
}

impl LifecycleOwner {
    pub(crate) fn new() -> Self {
        let (tx, alive) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let kill = CancellationToken::new();
        LifecycleOwner {
            handle: Some(Lifecycle {
                shutdown: shutdown.clone(),
                kill: kill.clone(),
                _alive: tx,
            }),
            shutdown,
            kill,
            alive,
        }
    }

    /// A handle for the request extensions. `None` after
    /// [`release`](Self::release).
    pub(crate) fn handle(&self) -> Option<Lifecycle> {
        self.handle.clone()
    }

    /// Asks every session to wind down.
    pub(crate) fn begin_shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Tells every session the grace period is over.
    pub(crate) fn kill(&self) {
        self.kill.cancel();
    }

    /// Gives up the owner's own handle, so that [`idle`](Self::idle) only
    /// waits for requests and sessions.
    pub(crate) fn release(&mut self) {
        self.handle = None;
    }

    /// Resolves when no handle is left. Call [`release`](Self::release)
    /// (and drop whatever else holds a handle, such as the router) first.
    pub(crate) async fn idle(&mut self) {
        while self.alive.recv().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn idle_waits_for_every_handle() {
        let mut owner = LifecycleOwner::new();
        let session = owner.handle().expect("a handle before release");
        owner.release();
        assert!(owner.handle().is_none());
        owner.begin_shutdown();
        assert!(session.shutdown.is_cancelled());
        assert!(!session.kill.is_cancelled());
        let waited = tokio::time::timeout(Duration::from_millis(30), owner.idle()).await;
        assert!(waited.is_err(), "a session is still alive");
        owner.kill();
        assert!(session.kill.is_cancelled());
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), owner.idle())
            .await
            .expect("nothing is left to wait for");
    }
}
