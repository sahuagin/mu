//! Desktop notifications via terminal escape (mu-solo-osc-notify-mbmn).
//!
//! The app writes a notification escape to its tty; the ENCLOSING
//! terminal raises the desktop notification under its own identity —
//! which is why such notifications carry the terminal's logo. The
//! app never touches the OS notification system, so this works over
//! SSH and through multiplexers that forward the escape.
//!
//! Channel choice is MEASURED, not assumed (operator bisection,
//! 2026-06-05, zellij 0.44.3 inside kitty, xfce4-notifyd daemon):
//!   OSC 9  (iTerm2 legacy)  — eaten by zellij, no popup
//!   BEL + title             — zellij consumes the bell as its own
//!                             visual flash; title forwards, no popup
//!   OSC 777 (urxvt notify)  — eaten by zellij, no popup
//!   OSC 99 (kitty native)   — FORWARDED by zellij, popup works
//! So: OSC 99, ST-terminated (`ESC ] 99 ; ; body ESC \`). If a
//! non-kitty terminal needs OSC 9 someday, that's a config knob to
//! add — not a reason to emit both (terminals supporting both would
//! double-pop).
//!
//! Policy lives in the caller (App): notify only when the terminal
//! is UNFOCUSED (crossterm focus events) and only for the main
//! session's turn boundaries — a notification about the terminal
//! you're already looking at is noise.

use std::io::Write;

/// Longest body we'll send. Terminals truncate anyway; keeping it
/// short keeps the popup readable.
const MAX_BODY_CHARS: usize = 160;

/// Emit an OSC 99 (kitty notification protocol) escape on stdout.
/// The body is sanitized: control characters (incl. ESC and BEL,
/// which would terminate or corrupt the sequence) are replaced with
/// spaces, and the result is capped at [`MAX_BODY_CHARS`].
pub fn notify(body: &str) {
    let clean: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_BODY_CHARS)
        .collect();
    let mut out = std::io::stdout();
    // ST-terminated OSC 99 with empty metadata — the single-shot
    // form (payload renders as the notification title). OSC
    // sequences don't move the cursor, so emitting between ratatui
    // frames is layout-safe.
    let _ = write!(out, "\x1b]99;;{clean}\x1b\\");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_chars() {
        let body = "done\x1b]9;injected\x07\nnext";
        let clean: String = body
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_BODY_CHARS)
            .collect();
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x07'));
        assert!(!clean.contains('\n'));
        assert!(clean.contains("]9;injected"));
    }

    #[test]
    fn sanitize_caps_length() {
        let body = "x".repeat(MAX_BODY_CHARS + 50);
        let clean: String = body.chars().take(MAX_BODY_CHARS).collect();
        assert_eq!(clean.chars().count(), MAX_BODY_CHARS);
    }
}
