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
//! WHO DECIDES whether to show — the layer with the focus info
//! (mu-solo-notify-pane-focus-jqnp):
//!   * Earlier (mu-solo-notify-occasion-56h0) the escape carried
//!     `o=invisible`, handing the show/suppress decision UP to kitty.
//!     But kitty only sees its OS window — it cannot tell one zellij
//!     pane from another inside the same window. So every in-window
//!     pane switch reads as "focused" and kitty stays silent; only
//!     app/tab switches ever notified. That was the bug.
//!   * zellij DOES proxy DECSET 1004 focus reporting per pane
//!     (operator re-measured 2026-06-06; the 56h0 "no ?1004" reading
//!     was wrong). mu-solo therefore has pane-granular focus via
//!     crossterm FocusGained/FocusLost (`App::terminal_focused`).
//!   * Fix: emit `o=always` so kitty shows the popup regardless of
//!     ITS window focus, and gate emission in the caller on
//!     `terminal_focused` — the layer that actually knows pane focus
//!     makes the decision (see [`should_notify`]). Resulting
//!     semantics, operator-specified: working in another zellij pane
//!     (same kitty window) notifies; sitting in the mu-solo pane
//!     stays silent.

use std::io::Write;

/// Longest body we'll send. Terminals truncate anyway; keeping it
/// short keeps the popup readable.
const MAX_BODY_CHARS: usize = 160;

/// Whether a turn-boundary notification should be emitted, given the
/// `tui.notifications` setting and the current PANE focus state.
///
/// This is the gate the bug turned on: notify only when the operator
/// is NOT looking at the mu-solo pane. `terminal_focused` is fed by
/// crossterm focus events, which zellij proxies per pane — so this is
/// a pane-level decision, not the window-level one kitty can make.
/// Paired with the `o=always` escape from [`format_notification`],
/// kitty shows whatever this lets through.
pub fn should_notify(notifications_enabled: bool, terminal_focused: bool) -> bool {
    notifications_enabled && !terminal_focused
}

/// Build the OSC 99 (kitty notification protocol) escape for `body`.
/// The body is sanitized: control characters (incl. ESC and BEL,
/// which would terminate or corrupt the sequence) are replaced with
/// spaces, and the result is capped at [`MAX_BODY_CHARS`].
///
/// Metadata `o=always`: kitty shows the notification regardless of
/// whether the emitting window is focused. The show/suppress decision
/// is made one layer down by [`should_notify`] using pane-level focus
/// — the layer that has the information kitty lacks (which zellij pane
/// is active). Returned as an owned `String` so the exact bytes are
/// unit-testable.
fn format_notification(body: &str) -> String {
    let clean: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_BODY_CHARS)
        .collect();
    // ST-terminated OSC 99; payload renders as the notification
    // title. OSC sequences don't move the cursor, so emitting
    // between ratatui frames is layout-safe.
    format!("\x1b]99;o=always;{clean}\x1b\\")
}

/// Emit an OSC 99 notification escape for `body` on stdout.
///
/// Callers must already have decided emission is wanted (see
/// [`should_notify`]); this only formats and writes the bytes.
pub fn notify(body: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(format_notification(body).as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_osc99_with_o_always() {
        let seq = format_notification("mu (claude-opus-4-8) is waiting for your input");
        // kitty path carries o=always so the popup shows regardless
        // of kitty window focus (mu-solo-notify-pane-focus-jqnp).
        assert!(seq.contains("o=always"), "missing o=always in {seq:?}");
        assert!(!seq.contains("o=invisible"), "stale o=invisible in {seq:?}");
        // Well-formed OSC 99: ESC ] 99 ; ... ST.
        assert!(seq.starts_with("\x1b]99;"));
        assert!(seq.ends_with("\x1b\\"));
        assert!(seq.contains("is waiting for your input"));
    }

    #[test]
    fn sanitize_strips_control_chars() {
        let seq = format_notification("done\x1b]9;injected\x07\nnext");
        // Only the framing ESC...ST may contain ESC; the body's
        // injected ESC/BEL/newline must be scrubbed to spaces.
        let body = seq
            .strip_prefix("\x1b]99;o=always;")
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .expect("framed OSC 99");
        assert!(!body.contains('\x1b'));
        assert!(!body.contains('\x07'));
        assert!(!body.contains('\n'));
        assert!(body.contains("]9;injected"));
    }

    #[test]
    fn sanitize_caps_length() {
        let seq = format_notification(&"x".repeat(MAX_BODY_CHARS + 50));
        let body = seq
            .strip_prefix("\x1b]99;o=always;")
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .expect("framed OSC 99");
        assert_eq!(body.chars().count(), MAX_BODY_CHARS);
    }

    #[test]
    fn gating_silent_when_pane_focused() {
        // Operator is looking at the mu-solo pane: stay silent.
        assert!(!should_notify(true, true));
    }

    #[test]
    fn gating_emits_when_pane_unfocused() {
        // Operator is in another pane/app: notify.
        assert!(should_notify(true, false));
    }

    #[test]
    fn gating_respects_disable() {
        // tui.notifications off: never notify, focused or not.
        assert!(!should_notify(false, false));
        assert!(!should_notify(false, true));
    }
}
