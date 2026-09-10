//! Safe IRC PRIVMSG framing.
//!
//! A single IRC protocol line, including its terminating `\r\n`, must not exceed
//! 512 bytes ([`LINE_BUDGET`]). This module renders a PRIVMSG for a target and
//! body and, when the body does not fit, splits it across several lines on UTF-8
//! scalar boundaries — never mid-character — marking every non-final line with
//! [`CONTINUATION_MARKER`] so a receiver can rejoin the pieces without content
//! loss. The complete serialized line is budgeted, so an optional `+mu.id`
//! message tag (emitted only when the server negotiated message-tags) is counted
//! against the 512 bytes too. Raw `\r`, `\n`, or `\0` in the target or body are
//! rejected rather than framed, so a body can never inject a second command.
//!
//! The target is held to a stricter rule than the body, because it is a command
//! PARAMETER rather than a payload: it must be a single channel or nick. A
//! target carrying `,`, a space, or a leading `:` injects no second command but
//! still changes what the message means — a target list fans the body out to
//! recipients the caller never named, and a space turns the remainder into the
//! trailing parameter, replacing the body. Those are refused too
//! ([`FramingError::InvalidTarget`]).
//!
//! The mesh id carried by that tag is attacker-influenced — it arrives verbatim
//! on the envelope — so it is never interpolated raw. An id outside the safe
//! grammar ([`is_safe_id_char`]) is either rendered with IRCv3 message-tag
//! escaping or refused outright, and the ESCAPED form is what the line budget
//! counts, so no id can end a tag early, start a second tag, or open a second
//! command.

/// Maximum bytes of one IRC line, INCLUDING the terminating `\r\n`.
pub const LINE_BUDGET: usize = 512;

/// Appended to every non-final line of a split body so a receiver knows more
/// follows. The original body is the concatenation of each line's payload with
/// this marker stripped from the non-final ones — no content is dropped.
pub const CONTINUATION_MARKER: &str = "\u{2026}"; // "…"

/// What to frame: the destination and the tag policy.
#[derive(Debug, Clone, Copy)]
pub struct FrameParams<'a> {
    /// The PRIVMSG target (channel or nick).
    pub target: &'a str,
    /// The mesh message id, emitted as the `+mu.id` client tag — but ONLY when
    /// `message_tags` is true. `None`, or tags not negotiated, means no tag.
    ///
    /// The value is NOT trusted: it is escaped or refused per
    /// [`FramingError::UnsafeMeshId`] before it reaches the wire.
    pub mesh_id: Option<&'a str>,
    /// Whether the server negotiated the `message-tags` capability. The tag is
    /// emitted only when this is true.
    pub message_tags: bool,
}

/// Why a body could not be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FramingError {
    /// The target was empty.
    #[error("empty PRIVMSG target")]
    EmptyTarget,
    /// The target or body contained a raw `\r`, `\n`, or `\0` — a CR/LF
    /// injection attempt, refused rather than silently stripped.
    #[error("target or body contains a control character (CR, LF, or NUL)")]
    ControlChar,
    /// The target was not a single channel or nick: it carried a `,` (which
    /// IRC reads as a target list, fanning the message out to recipients the
    /// caller never named), a space (which ends the target parameter, so the
    /// rest of the "target" becomes the trailing payload), or a leading `:`
    /// (which makes the target itself a trailing parameter).
    #[error("PRIVMSG target is not a single channel or nick")]
    InvalidTarget,
    /// The per-line overhead (tag + command + target + CRLF, plus the
    /// continuation marker for a split) left no room to make progress on the
    /// body — the budget cannot fit even a single scalar.
    #[error("line budget too small for this target/tag overhead")]
    BudgetTooSmall,
    /// The mesh id could not be rendered as an IRCv3 message-tag value: it held
    /// a `\0` (which tag escaping cannot represent at all) or some other
    /// character outside the safe id grammar with no defined escape. The message
    /// itself is fine — a caller that would rather deliver it untagged can
    /// re-frame with `mesh_id: None`.
    #[error("mesh id is not representable as an IRCv3 message-tag value")]
    UnsafeMeshId,
}

/// Whether a string carries a byte that would break or extend the IRC line.
fn has_control(s: &str) -> bool {
    s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

/// Whether `target` is a single channel-or-nick, i.e. one PRIVMSG target
/// parameter that means exactly what the caller wrote.
///
/// CR/LF/NUL are checked separately (and first) so their own error survives.
/// What is left are the three delimiters that silently change the message's
/// meaning without injecting a second command:
///
/// - `,` — IRC's target-list separator: `#private,#public` delivers to both.
/// - ` ` — the parameter separator: `#chan :x` frames as
///   `PRIVMSG #chan :x :<body>`, replacing the payload the caller passed.
/// - a leading `:` — marks a trailing parameter, so the target stops being a
///   target at all.
fn is_single_target(target: &str) -> bool {
    !target.starts_with(':') && !target.contains([',', ' '])
}

/// Characters a mesh id may carry into a tag value verbatim: ASCII
/// alphanumerics plus `-_.:`. That covers a ULID (`[0-9A-HJKMNP-TV-Z]{26}`), a
/// uuid, and the `role:id:sub` peer spellings without assuming any of them, and
/// excludes every byte that IRCv3 tag parsing gives a meaning to.
fn is_safe_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')
}

/// Render `id` as an IRCv3 message-tag value.
///
/// Safe characters pass through; the five characters tag escaping defines are
/// escaped (`;`→`\:`, space→`\s`, `\`→`\\`, CR→`\r`, LF→`\n`); anything else,
/// `\0` included, is refused with [`FramingError::UnsafeMeshId`] rather than
/// guessed at. `\0` has no escape sequence in the IRCv3 grammar, so there is no
/// safe rendering of it — it can only be rejected.
///
/// Callers budget the RETURNED string, never the input, so an id that grows
/// under escaping cannot push the line past [`LINE_BUDGET`].
fn escape_tag_value(id: &str) -> Result<String, FramingError> {
    let mut out = String::with_capacity(id.len());
    for c in id.chars() {
        match c {
            c if is_safe_id_char(c) => out.push(c),
            ';' => out.push_str(r"\:"),
            ' ' => out.push_str(r"\s"),
            '\\' => out.push_str(r"\\"),
            '\r' => out.push_str(r"\r"),
            '\n' => out.push_str(r"\n"),
            _ => return Err(FramingError::UnsafeMeshId),
        }
    }
    Ok(out)
}

/// Frame `body` as one or more complete PRIVMSG lines (each ending in `\r\n`),
/// none exceeding [`LINE_BUDGET`] bytes.
///
/// An empty body yields a single empty PRIVMSG. A body that fits yields one
/// line. A longer body is split on scalar boundaries, each non-final line
/// carrying [`CONTINUATION_MARKER`]; the pieces reassemble to the original.
pub fn frame_privmsg(params: &FrameParams, body: &str) -> Result<Vec<String>, FramingError> {
    if params.target.is_empty() {
        return Err(FramingError::EmptyTarget);
    }
    if has_control(params.target) || has_control(body) {
        return Err(FramingError::ControlChar);
    }
    if !is_single_target(params.target) {
        return Err(FramingError::InvalidTarget);
    }

    // The invariant part of every line: `[@+mu.id=<id> ]PRIVMSG <target> :`.
    // The id is escaped BEFORE it is measured, so the budget below counts the
    // bytes that actually reach the wire.
    let tag = match params.mesh_id.filter(|_| params.message_tags) {
        Some(id) => Some(format!("@+mu.id={} ", escape_tag_value(id)?)),
        None => None,
    };
    let prefix = format!(
        "{}PRIVMSG {} :",
        tag.as_deref().unwrap_or(""),
        params.target
    );
    // Bytes each line spends before any body: the prefix plus the CRLF.
    let overhead = prefix.len() + 2;
    if overhead >= LINE_BUDGET {
        return Err(FramingError::BudgetTooSmall);
    }
    // A final line carries no marker; a continued line reserves marker bytes.
    let body_final = LINE_BUDGET - overhead;
    let body_cont = body_final.saturating_sub(CONTINUATION_MARKER.len());

    let mut out = Vec::new();
    let mut rest = body;
    loop {
        if rest.len() <= body_final {
            out.push(format!("{prefix}{rest}\r\n"));
            break;
        }
        // Need a continuation line. It must fit at least one scalar of body
        // after reserving the marker, or we can never make progress.
        let end = floor_char_boundary(rest, body_cont);
        if end == 0 {
            return Err(FramingError::BudgetTooSmall);
        }
        let (head, tail) = rest.split_at(end);
        out.push(format!("{prefix}{head}{CONTINUATION_MARKER}\r\n"));
        rest = tail;
    }
    Ok(out)
}

/// Largest byte index `<= max` that is a char boundary of `s`. (Local copy so
/// the crate does not depend on the still-unstable `str::floor_char_boundary`.)
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}
