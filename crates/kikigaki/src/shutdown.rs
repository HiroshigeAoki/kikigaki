//! Idempotent process-shutdown coordination.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::atomic::{AtomicU8, Ordering};

const RUNNING: u8 = 0;
const JOINING: u8 = 1;
#[cfg(any(test, target_os = "macos"))]
const EXITING: u8 = 2;

pub(crate) struct ShutdownState(AtomicU8);

impl ShutdownState {
    pub(crate) const fn new() -> Self {
        Self(AtomicU8::new(RUNNING))
    }

    /// Wins the shutdown race exactly once.
    pub(crate) fn begin(&self) -> bool {
        let began = self
            .0
            .compare_exchange(RUNNING, JOINING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if began {
            tracing::info!(from = "Running", to = "Joining", "shutdown state changed");
        }
        began
    }

    pub(crate) fn is_running(&self) -> bool {
        self.0.load(Ordering::SeqCst) == RUNNING
    }

    #[cfg(any(test, target_os = "macos"))]
    pub(crate) fn mark_exiting(&self) {
        self.0.store(EXITING, Ordering::SeqCst);
        tracing::info!(from = "Joining", to = "Exiting", "shutdown state changed");
    }

    /// Blocks until `mark_exiting` has run (true) or the timeout elapses (false).
    ///
    /// Shutdown-only path, so a coarse poll is fine; no condvar needed.
    #[cfg(any(test, target_os = "macos"))]
    pub(crate) fn wait_until_exiting(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.0.load(Ordering::SeqCst) == EXITING {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn begin_shutdown(app: &tauri::AppHandle) {
    begin_shutdown_with_code(app, 0);
}

#[cfg(target_os = "macos")]
fn join_controller_and_log(state: &crate::shell::ShellState) {
    if let Some((controller, _death_monitor)) = state.controller.lock().unwrap().take() {
        if controller.join(std::time::Duration::from_secs(5)) {
            tracing::info!("controller joined");
        } else {
            tracing::error!("controller did not join within 5s; exiting anyway");
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn begin_shutdown_with_code(app: &tauri::AppHandle, code: i32) {
    use tauri::Manager;

    let Some(state) = app.try_state::<std::sync::Arc<crate::shell::ShellState>>() else {
        return;
    };
    if !state.shutdown.begin() {
        return;
    }
    state.stop_onboarding_poll();
    state.client.send(crate::controller::ControllerCmd::Quit);
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("shutdown-join".into())
        .spawn(move || {
            if let Some(shell_state) = app.try_state::<std::sync::Arc<crate::shell::ShellState>>() {
                join_controller_and_log(&shell_state);
                shell_state.shutdown.mark_exiting();
            }
            let exit_app = app.clone();
            if let Err(error) = app.run_on_main_thread(move || {
                tracing::info!(code, "exiting");
                exit_app.exit(code);
            }) {
                tracing::error!(%error, "failed to schedule process exit");
            }
        })
        .expect("spawn shutdown join helper");
}

/// Synchronous variant for quit paths that never emit `ExitRequested`: an NSApp terminate (Apple
/// event, logout, Activity Monitor quit) reaches us through tao's `applicationWillTerminate` →
/// `RunEvent::Exit`, after which the process ends as soon as the callback returns. The only way to
/// honor "every quit source joins the controller" there is to block inside the callback.
#[cfg(target_os = "macos")]
pub(crate) fn finish_synchronously(app: &tauri::AppHandle, source: &'static str) {
    use tauri::Manager;

    let Some(state) = app.try_state::<std::sync::Arc<crate::shell::ShellState>>() else {
        return;
    };
    if !state.shutdown.begin() {
        // A `begin_shutdown` join thread is already in flight (or finished). Returning immediately
        // here would let tao end the process the moment this callback returns, killing that thread
        // mid-join — observed as a mid-download Cmd+Q exiting right after "Joining" with no
        // further log lines and the controller never joined. Block until it marks Exiting
        // (controller join is bounded at 5 s, so 7 s covers it) before letting the process die.
        tracing::info!(
            source,
            "quit signal while shutdown already in flight; waiting for join"
        );
        if state
            .shutdown
            .wait_until_exiting(std::time::Duration::from_secs(7))
        {
            tracing::info!(source, code = 0, "in-flight shutdown completed; exiting");
        } else {
            tracing::error!(
                source,
                "in-flight shutdown did not finish within 7s; exiting anyway"
            );
        }
        return;
    }
    tracing::info!(source, "quit requested");
    state.stop_onboarding_poll();
    state.client.send(crate::controller::ControllerCmd::Quit);
    join_controller_and_log(&state);
    state.shutdown.mark_exiting();
    tracing::info!(code = 0, "exiting");
}

#[cfg(test)]
mod tests {
    use super::ShutdownState;
    use std::time::Duration;

    #[test]
    fn wait_until_exiting_returns_true_when_marked_from_another_thread() {
        let state = ShutdownState::new();
        assert!(state.begin());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                state.mark_exiting();
            });
            assert!(state.wait_until_exiting(Duration::from_secs(5)));
        });
    }

    #[test]
    fn wait_until_exiting_times_out_when_never_marked() {
        let state = ShutdownState::new();
        assert!(state.begin());
        assert!(!state.wait_until_exiting(Duration::from_millis(50)));
    }

    #[test]
    fn wait_until_exiting_returns_immediately_when_already_exiting() {
        let state = ShutdownState::new();
        assert!(state.begin());
        state.mark_exiting();
        let started = std::time::Instant::now();
        assert!(state.wait_until_exiting(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn begin_wins_exactly_once_and_marks_shutdown_non_running() {
        let state = ShutdownState::new();
        assert!(state.is_running());
        assert!(state.begin());
        assert!(!state.is_running());
        assert!(!state.begin());
        state.mark_exiting();
        assert_eq!(
            state.0.load(std::sync::atomic::Ordering::SeqCst),
            super::EXITING
        );
    }
}
