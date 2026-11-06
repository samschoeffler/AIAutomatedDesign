//! Console output that never makes the caller wait.
//!
//! Writing to a Windows console can block: with QuickEdit on, clicking in the window
//! starts a selection and pauses every write until you press Enter or Esc. If a bridge
//! request printed directly (worse, while holding the app lock), one stray click froze
//! the whole app and Claude's calls timed out. So everything goes through [`say`], which
//! hands the text to a printer thread and returns at once.

use std::io::Write;
use std::sync::{mpsc, Mutex, OnceLock};

static OUT: OnceLock<mpsc::Sender<String>> = OnceLock::new();

/// Held by the printer while it writes. Tests take it to stand in for a frozen console.
pub(crate) static WRITING: Mutex<()> = Mutex::new(());

/// Print `text` as is (include your own newlines). Never blocks.
pub fn say(text: impl Into<String>) {
    let out = OUT.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for text in rx {
                let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(text.as_bytes());
                let _ = stdout.flush();
            }
        });
        tx
    });
    let _ = out.send(text.into());
}

/// `say` with a newline on the end.
pub fn say_line(text: impl Into<String>) {
    let mut text = text.into();
    text.push('\n');
    say(text);
}
