//! Wake-up helpers. v0 exposes a tokio-native SIGUSR2 stream for `caucus
//! watch` to react to tmux-hook nudges (a future iteration may install a
//! tmux hook that fires `kill -USR2 <pid>` per `docs/dmux-analysis.md` §6).
//! We currently prefer the sentinel-file IPC for inter-process signalling;
//! SIGUSR2 is a small belt for tools that want to poke a running watcher
//! without writing a sentinel file.

use thiserror::Error;
use tokio::signal::unix::{Signal, SignalKind, signal};

/// Errors from setting up signal listeners.
#[derive(Debug, Error)]
pub enum SignalError {
    #[error("install signal handler ({kind:?}): {source}")]
    Install {
        kind: SignalKindLabel,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum SignalKindLabel {
    Usr2,
    Interrupt,
}

/// Async stream over SIGUSR2 occurrences.
pub struct Usr2Stream {
    inner: Signal,
}

impl Usr2Stream {
    pub fn new() -> Result<Self, SignalError> {
        let inner = signal(SignalKind::user_defined2()).map_err(|source| SignalError::Install {
            kind: SignalKindLabel::Usr2,
            source,
        })?;
        Ok(Self { inner })
    }

    /// Wait for the next SIGUSR2 (or `None` if the listener was torn down).
    pub async fn recv(&mut self) -> Option<()> {
        self.inner.recv().await
    }
}

/// Convenience: future that completes on the first SIGINT.
pub async fn graceful_shutdown() -> Result<(), SignalError> {
    let mut sig = signal(SignalKind::interrupt()).map_err(|source| SignalError::Install {
        kind: SignalKindLabel::Interrupt,
        source,
    })?;
    sig.recv().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn usr2_stream_installs_handler() {
        let _ = Usr2Stream::new().expect("install usr2 handler");
        let _ = Usr2Stream::new().expect("re-install usr2 handler");
    }
}
