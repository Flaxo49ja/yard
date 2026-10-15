//! v0.4 — raw single-key input for the session view ([P] Pause, [R] Reading,
//! [X] Emergency exit). POSIX terminals only (Linux/macOS), via `stty`:
//! save the current settings with `stty -g`, flip to raw+nonblocking with
//! `stty raw -echo min 0 time 0`, then one-byte reads answer instantly.
//!
//! No new dependency: stty is POSIX. On Windows, or whenever stdin is not a
//! terminal (piped/scripted runs), `enable()` returns None and main.rs
//! keeps the honest prose hints — nothing pretends to have keys.

use std::io::{IsTerminal, Read};
use std::sync::OnceLock;

/// Key that means "emergency exit" (bank partial as a bail).
pub const KEY_EXIT: char = 'x';
/// Key that toggles the manual pause (clock stops, no credit accrues).
pub const KEY_PAUSE: char = 'p';
/// Key that grants +1 minute of reading allowance.
pub const KEY_READ: char = 'r';

static SAVED_STTY: OnceLock<String> = OnceLock::new();

fn stty(args: &str) -> Option<std::process::ExitStatus> {
    std::process::Command::new("stty")
        .args(args.split_whitespace())
        .stdin(std::process::Stdio::inherit())
        .status()
        .ok()
        .filter(|s| s.success())
}

/// Restore the terminal state saved by `enable()` (safe to call anywhere,
/// no-op if raw mode was never entered).
pub fn restore() {
    if let Some(orig) = SAVED_STTY.get() {
        stty(orig);
    }
}

pub struct RawKeys;

impl Drop for RawKeys {
    fn drop(&mut self) {
        restore();
    }
}

impl RawKeys {
    pub fn enable() -> Option<Self> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let orig = {
            let out = std::process::Command::new("stty")
                .arg("-g")
                .stdin(std::process::Stdio::inherit())
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                return None;
            }
            s
        };
        SAVED_STTY.get_or_init(|| orig.clone());
        stty("raw -echo min 0 time 0")?;
        Some(Self)
    }

    /// Nonblocking read: the key that arrived since last tick, or None.
    /// Arrow/multi-byte escape sequences are consumed and ignored (they are
    /// navigation, not commands). Case-insensitive for a-z.
    pub fn try_read(&self) -> Option<char> {
        let mut buf = [0u8; 16];
        let n = match std::io::stdin().read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => n,
        };
        if buf[..n].contains(&0x1b) {
            return None; // escape sequence: ignore the whole burst
        }
        let c = buf[0] as char;
        if c.is_ascii_alphabetic() {
            Some(c.to_ascii_lowercase())
        } else if c.is_ascii_whitespace() {
            None
        } else {
            Some(c) // digits etc. are read but ignored by main
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_constants_are_single_lowercase_chars() {
        assert!(KEY_PAUSE.is_ascii_alphabetic());
        assert!(KEY_READ.is_ascii_alphabetic());
        assert!(KEY_EXIT.is_ascii_alphabetic());
        assert_ne!(KEY_PAUSE, KEY_READ);
        assert_ne!(KEY_READ, KEY_EXIT);
    }
}
