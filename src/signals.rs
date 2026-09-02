//! Signal listeners that survive the gaps between polls.
//!
//! Tokio *discards* a delivered signal when nothing is registered for it at
//! broadcast time — it does not defer it. So the registration has to exist
//! before the window it protects, and it has to outlive every future built
//! from it: a listener created inside a `select!` arm is rebuilt on every loop
//! iteration, and one created after the critical section starts leaves that
//! section running under the default terminate action.

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    Interrupt,
    Terminate,
}

impl ShutdownSignal {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }
}

/// Listens for the signals that mean "wind down now".
pub(crate) struct ShutdownListener {
    #[cfg(unix)]
    sigterm: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
    #[cfg(not(unix))]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl ShutdownListener {
    /// SIGTERM and SIGINT (Ctrl+C on Windows), for a process that owns its own
    /// lifetime and should shut down cleanly on either.
    pub(crate) fn new() -> Result<Self> {
        Self::build(true)
    }

    /// Ctrl+C only.
    ///
    /// For a command that merely needs to unwind a critical section and then
    /// keep running. Registering SIGTERM there would be a regression rather
    /// than a hardening: tokio's registration is process-wide and permanent, so
    /// after the critical section the command would go on ignoring `kill` for
    /// as long as it lives.
    pub(crate) fn interrupt_only() -> Result<Self> {
        Self::build(false)
    }

    fn build(include_terminate: bool) -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                sigterm: include_terminate
                    .then(|| signal(SignalKind::terminate()))
                    .transpose()?,
                sigint: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        {
            // Windows has no SIGTERM; console Ctrl+C is the only equivalent, so
            // both constructors listen for the same thing.
            let _ = include_terminate;
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    /// Resolves once a signal has been received. Safe to cancel: the
    /// registration lives in `self`, so a signal that arrives while this future
    /// is not being polled is still observed by the next call.
    pub(crate) async fn recv(&mut self) {
        let _ = self.recv_signal().await;
    }

    pub(crate) async fn recv_signal(&mut self) -> ShutdownSignal {
        #[cfg(unix)]
        {
            match self.sigterm.as_mut() {
                Some(sigterm) => {
                    tokio::select! {
                        _ = sigterm.recv() => ShutdownSignal::Terminate,
                        _ = self.sigint.recv() => ShutdownSignal::Interrupt,
                    }
                }
                // `None` only means the stream ended, which tokio does not do
                // for a live registration; treating it as a shutdown is the
                // safe reading either way.
                None => {
                    self.sigint.recv().await;
                    ShutdownSignal::Interrupt
                }
            }
        }
        #[cfg(not(unix))]
        {
            self.ctrl_c.recv().await;
            ShutdownSignal::Interrupt
        }
    }
}

/// Serialises every test that raises a signal at this process.
///
/// A raise is process-wide, so two such tests running on different threads of
/// the same test binary observe each other's signals. This module and
/// `commands::launch` both have one, and they compile into the same binary.
#[cfg(test)]
pub(crate) static RAISE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(all(test, unix))]
mod tests {
    use super::{RAISE_LOCK, ShutdownListener};
    use std::time::Duration;
    use tokio::signal::unix::{SignalKind, signal};

    /// `daemon stop` sends a single SIGTERM. The daemon's select loop spends a
    /// large share of every second inside a branch body (the poll branch does
    /// HTTP round trips), and during that time nothing polls the shutdown
    /// branch. Tokio drops a delivered signal outright when no listener is
    /// registered at broadcast time, so the listener has to survive across
    /// loop iterations rather than be rebuilt inside `select!`.
    #[tokio::test]
    async fn shutdown_listener_catches_a_signal_raised_while_it_is_not_polled() {
        let _raise = RAISE_LOCK.lock().await;
        let mut listener = ShutdownListener::new().expect("shutdown listener");

        // A second listener registered up front turns "tokio has finished
        // broadcasting the signal" into an awaitable event, so the assertion
        // below never depends on sleeping long enough.
        let mut witness = signal(SignalKind::terminate()).expect("witness listener");

        // SAFETY: raising SIGTERM at our own process. Both listeners above are
        // registered first, so tokio's handler is installed and the default
        // terminate action cannot fire.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        witness.recv().await;

        // `listener` was never polled before the broadcast -- exactly the state
        // the daemon loop is in while a poll body runs.
        tokio::time::timeout(Duration::from_secs(5), listener.recv())
            .await
            .expect("a SIGTERM delivered while the loop was busy must not be lost");
    }

    /// An interrupt-only listener must ignore SIGTERM outright rather than
    /// register a handler for it: registering is irreversible for the life of
    /// the process, so a command that only wants to unwind a short critical
    /// section would otherwise stop responding to `kill` forever after.
    #[tokio::test]
    async fn interrupt_only_listener_does_not_answer_sigterm() {
        let _raise = RAISE_LOCK.lock().await;
        let mut listener = ShutdownListener::interrupt_only().expect("interrupt listener");

        // Registered so the raise below cannot terminate this test process --
        // proving the point requires surviving the signal, not the default
        // action for it.
        let mut witness = signal(SignalKind::terminate()).expect("witness listener");

        // SAFETY: raising SIGTERM at our own process, with `witness`
        // registered first so the default terminate action cannot fire.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        witness.recv().await;

        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.recv())
                .await
                .is_err(),
            "an interrupt-only listener must leave SIGTERM to the rest of the program"
        );
    }
}
