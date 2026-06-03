//! Semantic transcript model for mu-solo.
//!
//! This is deliberately independent of the rendered ratatui/crossterm
//! screen. Copy/export operate on these records, not on terminal cells.

use std::fmt::Write as _;

use crate::app::TurnRoute;
use crate::render::TurnItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptKind {
    User,
    Assistant,
    Notice,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptBlock {
    pub kind: TranscriptKind,
    pub label: String,
    pub body: String,
}

impl TranscriptBlock {
    pub fn new(kind: TranscriptKind, label: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            label: label.into(),
            body: body.into(),
        }
    }

    pub fn user(route: TurnRoute, body: impl Into<String>) -> Self {
        Self::new(TranscriptKind::User, route.you_label(), body)
    }

    pub fn assistant(route: TurnRoute, items: &[TurnItem]) -> Self {
        Self::new(
            TranscriptKind::Assistant,
            route.header_label(),
            render_turn_items_plain(items),
        )
    }

    pub fn notice(label: impl Into<String>, body: impl Into<String>) -> Self {
        Self::new(TranscriptKind::Notice, label, body)
    }

    pub fn error(body: impl Into<String>) -> Self {
        Self::new(TranscriptKind::Error, "ERROR", body)
    }
}

#[derive(Debug, Default, Clone)]
pub struct Transcript {
    blocks: Vec<TranscriptBlock>,
}

impl Transcript {
    pub fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    pub fn push(&mut self, block: TranscriptBlock) {
        self.blocks.push(block);
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn get(&self, idx: usize) -> Option<&TranscriptBlock> {
        self.blocks.get(idx)
    }

    pub fn last(&self) -> Option<&TranscriptBlock> {
        self.blocks.last()
    }

    pub fn last_matching(&self, kind: TranscriptKind) -> Option<&TranscriptBlock> {
        self.blocks.iter().rev().find(|block| block.kind == kind)
    }

    pub fn render_all_plain(&self) -> String {
        render_blocks_plain(&self.blocks)
    }
}

pub fn render_blocks_plain(blocks: &[TranscriptBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        let _ = writeln!(out, "## {}", block.label);
        if !block.body.is_empty() {
            out.push_str(block.body.trim_end());
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

pub fn render_turn_items_plain(items: &[TurnItem]) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            TurnItem::Text(text) => {
                out.push_str(text.trim_end());
                out.push('\n');
            }
            TurnItem::ToolCall {
                display_name,
                primary_arg,
            } => {
                if primary_arg.is_empty() {
                    let _ = writeln!(out, "[tool] {display_name}");
                } else {
                    let _ = writeln!(out, "[tool] {display_name}({primary_arg})");
                }
            }
            TurnItem::ToolResult { kind, text } => {
                let _ = writeln!(out, "[tool result: {kind}]");
                if !text.is_empty() {
                    out.push_str(text.trim_end());
                    out.push('\n');
                }
            }
            TurnItem::Error(msg) => {
                let _ = writeln!(out, "[error] {msg}");
            }
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_render_separates_blocks() {
        let mut transcript = Transcript::new();
        transcript.push(TranscriptBlock::new(TranscriptKind::User, "you", "hello"));
        transcript.push(TranscriptBlock::new(
            TranscriptKind::Assistant,
            "assistant",
            "world",
        ));
        let plain = transcript.render_all_plain();
        assert!(plain.contains("## you\nhello\n\n## assistant\nworld"));
    }

    #[test]
    fn turn_items_plain_keeps_tool_output() {
        let items = vec![
            TurnItem::Text("hi".into()),
            TurnItem::ToolCall {
                display_name: "Bash".into(),
                primary_arg: "echo ok".into(),
            },
            TurnItem::ToolResult {
                kind: "ok".into(),
                text: "ok".into(),
            },
        ];
        let plain = render_turn_items_plain(&items);
        assert!(plain.contains("hi"));
        assert!(plain.contains("[tool] Bash(echo ok)"));
        assert!(plain.contains("[tool result: ok]\nok"));
    }
}
