//! Writing to the standard streams without panicking when they are closed
//! (`println!` panics on a broken pipe; a server whose parent stopped
//! reading its output must keep serving).

use std::io::Write;

/// Writes `text` and a newline to stdout and flushes, so that a script
/// reading a pipe sees the line at once.
pub fn stdout_line(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

/// Writes `text` to stdout as it is (it brings its own newlines).
pub fn stdout_text(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Writes `text` and a newline to stderr.
pub fn stderr_line(text: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{text}");
}

/// Writes `text` to stderr as it is (it brings its own newlines).
pub fn stderr_text(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}
