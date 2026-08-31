use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use kikigaki_core::config::PasteMethod;

/// How long the pasted text stays on the clipboard before the previous content is restored.
/// The target app reads the clipboard when it handles Cmd+V, which is usually well within
/// this window.
const RESTORE_DELAY: Duration = Duration::from_millis(300);

pub fn paste(text: &str, method: PasteMethod) -> anyhow::Result<()> {
    let mut enigo = Enigo::new(&Settings::default())
        .context("paste failed: grant Accessibility to kikigaki in System Settings")?;
    match method {
        PasteMethod::Clipboard => paste_via_clipboard(text, &mut enigo),
        PasteMethod::Type => enigo
            .text(text)
            .context("paste failed: grant Accessibility to kikigaki in System Settings"),
    }
}

/// Tracks the clipboard content that has to be restored after a burst of pastes.
///
/// Restoring runs on a background thread `RESTORE_DELAY` after the *last* paste, so the
/// event loop is never blocked and consecutive per-segment pastes keep the original
/// clipboard content (not the previous segment's text) as the value to restore.
#[derive(Default)]
struct RestoreQueue {
    /// `Some(original)` while a restore is pending; `original` is `None` when the clipboard
    /// held no text before the burst.
    pending: Option<Option<String>>,
    seq: u64,
}

impl RestoreQueue {
    /// Registers a paste. `read_original` is only called when no restore is pending, i.e. the
    /// clipboard still holds the user's own content. Returns the ticket for `finish`.
    fn begin(&mut self, read_original: impl FnOnce() -> Option<String>) -> u64 {
        if self.pending.is_none() {
            self.pending = Some(read_original());
        }
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Called when the delay for `ticket` elapses. Returns the content to restore only if no
    /// newer paste superseded this ticket.
    fn finish(&mut self, ticket: u64) -> Option<Option<String>> {
        if self.seq == ticket {
            self.pending.take()
        } else {
            None
        }
    }
}

fn restore_queue() -> &'static Mutex<RestoreQueue> {
    static QUEUE: OnceLock<Mutex<RestoreQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(RestoreQueue::default()))
}

fn paste_via_clipboard(text: &str, enigo: &mut Enigo) -> anyhow::Result<()> {
    let mut clipboard = Clipboard::new().context("open clipboard")?;
    let ticket = {
        let mut queue = restore_queue().lock().unwrap_or_else(|e| e.into_inner());
        queue.begin(|| clipboard.get_text().ok())
    };
    clipboard.set_text(text).context("set clipboard text")?;

    let paste_result = (|| -> anyhow::Result<()> {
        enigo
            .key(Key::Meta, Direction::Press)
            .context("press Command for paste")?;
        let click_result = enigo
            .key(Key::Unicode('v'), Direction::Click)
            .context("press V for paste");
        let release_result = enigo
            .key(Key::Meta, Direction::Release)
            .context("release Command after paste");
        click_result?;
        release_result
    })();

    thread::spawn(move || {
        thread::sleep(RESTORE_DELAY);
        let original = {
            let mut queue = restore_queue().lock().unwrap_or_else(|e| e.into_inner());
            queue.finish(ticket)
        };
        if let Some(Some(previous)) = original {
            match Clipboard::new() {
                Ok(mut clipboard) => {
                    if let Err(error) = clipboard.set_text(previous) {
                        tracing::warn!(%error, "failed to restore clipboard text");
                    }
                }
                Err(error) => tracing::warn!(%error, "failed to open clipboard for restore"),
            }
        }
    });

    paste_result.context("paste failed: grant Accessibility to kikigaki in System Settings")
}

#[cfg(test)]
mod tests {
    use super::RestoreQueue;

    #[test]
    fn single_paste_restores_original_after_delay() {
        let mut queue = RestoreQueue::default();
        let ticket = queue.begin(|| Some("original".into()));
        assert_eq!(queue.finish(ticket), Some(Some("original".into())));
        assert_eq!(queue.finish(ticket), None, "restore happens once");
    }

    #[test]
    fn burst_keeps_the_original_and_restores_after_the_last_paste() {
        let mut queue = RestoreQueue::default();
        let first = queue.begin(|| Some("original".into()));
        let second = queue.begin(|| panic!("clipboard must not be re-read mid-burst"));
        assert_eq!(
            queue.finish(first),
            None,
            "superseded ticket restores nothing"
        );
        assert_eq!(queue.finish(second), Some(Some("original".into())));
    }

    #[test]
    fn empty_clipboard_before_the_paste_restores_nothing_but_clears_pending() {
        let mut queue = RestoreQueue::default();
        let ticket = queue.begin(|| None);
        assert_eq!(queue.finish(ticket), Some(None));
        let next = queue.begin(|| Some("later".into()));
        assert_eq!(queue.finish(next), Some(Some("later".into())));
    }
}
