//! ASCII OS-memory-map renderer for context window attribution.
//!
//! Pure rendering: `render(&ContextAttribution, width) -> String`.
//! No TTY detection, no color, no side effects.

use std::fmt::Write;

use crate::context_attribution::{ContextAttribution, ToolAttribution};

const BAR_FULL: char = '█';
const BAR_EMPTY: char = '░';

/// Render a context-window attribution as an ASCII region map.
///
/// `width` is the total terminal width in columns. The output is
/// designed to look good at 80 and 120 columns.
pub fn render(attr: &ContextAttribution, width: u16) -> String {
    let mut out = String::new();

    let w = width as usize;
    let total_input = attr.total_input_tokens;

    // Header: overall usage bar
    render_header(&mut out, attr, w);

    // Region tree
    out.push('\n');
    render_region_tree(&mut out, attr, w, total_input);

    // Cache hit ratio
    if attr.cache_read_tokens > 0 || attr.cache_creation_tokens > 0 {
        out.push('\n');
        render_cache_info(&mut out, attr);
    }

    // Top tool consumers
    if !attr.tool_attribution.is_empty() {
        out.push('\n');
        render_top_consumers(&mut out, &attr.tool_attribution, w);
    }

    out
}

fn render_header(out: &mut String, attr: &ContextAttribution, _w: usize) {
    let total = attr.total_input_tokens;
    let max = attr.window_max.unwrap_or(200_000);

    let bar_width: usize = 30;
    let filled = if max > 0 {
        ((total as f64 / max as f64) * bar_width as f64).round() as usize
    } else {
        0
    };
    let filled = filled.min(bar_width);
    let empty = bar_width - filled;
    let pct = if max > 0 {
        (total as f64 / max as f64) * 100.0
    } else {
        0.0
    };

    let _ = write!(
        out,
        "Context window: {} tokens  [{}{}] {} / {} ({:.1}%)",
        format_tokens(max),
        BAR_FULL.to_string().repeat(filled),
        BAR_EMPTY.to_string().repeat(empty),
        format_tokens(total),
        format_tokens(max),
        pct,
    );
}

/// Region breakdown as a box-drawing tree.
///
/// We synthesize regions from the last model call's message counts
/// (the "current snapshot" of context) and from cumulative usage.
fn render_region_tree(out: &mut String, attr: &ContextAttribution, w: usize, total_input: u64) {
    struct Region {
        label: String,
        tokens: u64,
        children: Vec<Region>,
        is_free: bool,
    }

    let last_call = attr.model_calls.last();
    let last_usage = last_call.and_then(|mc| mc.usage);

    // Derive region breakdown. When we have a last model call's usage,
    // we use its input_tokens as the "current snapshot" total. Otherwise
    // fall back to cumulative.
    let snapshot_input = last_usage.map(|u| u.input_tokens).unwrap_or(total_input);

    // We can't know exact per-region token counts without a tokenizer,
    // so we show what we DO know: message counts and tool counts from
    // the latest ContextAssembly, and raw token numbers from usage.
    // We attribute proportionally when needed.

    let mut regions: Vec<Region> = Vec::new();

    if let Some(mc) = last_call {
        // Estimate: system prompt + tool schemas are typically ~10-15% of input
        // We'll show the message structure from ContextAssembly
        let tool_schema_estimate = mc.tool_count as u64 * 150;
        let msg_tokens = snapshot_input.saturating_sub(tool_schema_estimate);

        // Conversation region with children
        let user_ratio = if mc.message_count > 0 {
            mc.user_message_count as f64 / mc.message_count as f64
        } else {
            0.0
        };
        let asst_ratio = if mc.message_count > 0 {
            mc.assistant_message_count as f64 / mc.message_count as f64
        } else {
            0.0
        };
        let tool_result_ratio = if mc.message_count > 0 {
            mc.tool_result_count as f64 / mc.message_count as f64
        } else {
            0.0
        };

        let user_tokens = (msg_tokens as f64 * user_ratio).round() as u64;
        let asst_tokens = (msg_tokens as f64 * asst_ratio).round() as u64;
        let tool_result_tokens = (msg_tokens as f64 * tool_result_ratio).round() as u64;

        let conversation_tokens = user_tokens + asst_tokens + tool_result_tokens;

        if mc.tool_count > 0 {
            regions.push(Region {
                label: format!("Tool schemas ({})", mc.tool_count),
                tokens: tool_schema_estimate,
                children: Vec::new(),
                is_free: false,
            });
        }

        let mut conv_children = Vec::new();
        if mc.user_message_count > 0 {
            conv_children.push(Region {
                label: format!("User messages ({})", mc.user_message_count),
                tokens: user_tokens,
                children: Vec::new(),
                is_free: false,
            });
        }
        if mc.tool_result_count > 0 {
            conv_children.push(Region {
                label: format!("Tool results ({})", mc.tool_result_count),
                tokens: tool_result_tokens,
                children: Vec::new(),
                is_free: false,
            });
        }
        if mc.assistant_message_count > 0 {
            conv_children.push(Region {
                label: format!("Assistant messages ({})", mc.assistant_message_count),
                tokens: asst_tokens,
                children: Vec::new(),
                is_free: false,
            });
        }

        regions.push(Region {
            label: format!("Conversation ({} msgs)", mc.message_count),
            tokens: conversation_tokens,
            children: conv_children,
            is_free: false,
        });
    } else if total_input > 0 {
        regions.push(Region {
            label: "Input tokens".into(),
            tokens: total_input,
            children: Vec::new(),
            is_free: false,
        });
    }

    // Output tokens
    if attr.total_output_tokens > 0 {
        regions.push(Region {
            label: "Output tokens".into(),
            tokens: attr.total_output_tokens,
            children: Vec::new(),
            is_free: false,
        });
    }

    // Free space
    if let Some(max) = attr.window_max {
        let used = total_input;
        if used < max {
            regions.push(Region {
                label: "Free".into(),
                tokens: max - used,
                children: Vec::new(),
                is_free: true,
            });
        }
    }

    // Find max tokens for bar scaling
    let max_tokens = regions.iter().map(|r| r.tokens).max().unwrap_or(1).max(1);

    // Compute column layout
    let label_width = 30.min(w.saturating_sub(40));
    let token_col_width = 12;
    let pct_col_width = 8;
    let bar_budget = w
        .saturating_sub(4) // tree prefix "├─ "
        .saturating_sub(label_width)
        .saturating_sub(2) // spacing
        .saturating_sub(token_col_width)
        .saturating_sub(pct_col_width)
        .max(5);

    let total_for_pct = if let Some(window_max) = attr.window_max {
        window_max
    } else {
        total_input.max(1)
    };

    for (i, region) in regions.iter().enumerate() {
        let is_last = i == regions.len() - 1;
        let prefix = if i == 0 {
            "┌─"
        } else if is_last {
            "└─"
        } else {
            "├─"
        };

        render_region_line(
            out,
            prefix,
            &region.label,
            region.tokens,
            region.is_free,
            LineLayout {
                label_width,
                bar_budget,
                token_col_width,
                max_tokens,
                total_for_pct,
            },
        );

        // Children
        for (j, child) in region.children.iter().enumerate() {
            let child_is_last = j == region.children.len() - 1;
            let child_prefix = if is_last {
                if child_is_last {
                    "   └─"
                } else {
                    "   ├─"
                }
            } else if child_is_last {
                "│  └─"
            } else {
                "│  ├─"
            };
            render_region_line(
                out,
                child_prefix,
                &child.label,
                child.tokens,
                false,
                LineLayout {
                    label_width: label_width.saturating_sub(3),
                    bar_budget,
                    token_col_width,
                    max_tokens,
                    total_for_pct,
                },
            );
        }
    }
}

/// Column dimensions for [`render_region_line`], bundled to keep the line
/// renderer under the positional-arg limit. `Copy` — all small scalars.
#[derive(Clone, Copy)]
struct LineLayout {
    label_width: usize,
    bar_budget: usize,
    token_col_width: usize,
    max_tokens: u64,
    total_for_pct: u64,
}

fn render_region_line(
    out: &mut String,
    prefix: &str,
    label: &str,
    tokens: u64,
    is_free: bool,
    layout: LineLayout,
) {
    let LineLayout {
        label_width,
        bar_budget,
        token_col_width,
        max_tokens,
        total_for_pct,
    } = layout;
    let bar_len = if max_tokens > 0 {
        ((tokens as f64 / max_tokens as f64) * bar_budget as f64)
            .round()
            .max(if tokens > 0 { 1.0 } else { 0.0 }) as usize
    } else {
        0
    };
    let bar_char = if is_free { BAR_EMPTY } else { BAR_FULL };
    let bar: String = bar_char.to_string().repeat(bar_len);

    let pct = if total_for_pct > 0 {
        (tokens as f64 / total_for_pct as f64) * 100.0
    } else {
        0.0
    };

    let truncated_label = if label.len() > label_width {
        &label[..label_width]
    } else {
        label
    };

    let token_str = format!("{} tokens", format_tokens(tokens));

    let _ = writeln!(
        out,
        "{prefix} {:<lw$} {:<bw$} {:>tw$}  ({:>5.1}%)",
        truncated_label,
        bar,
        token_str,
        pct,
        lw = label_width,
        bw = bar_budget,
        tw = token_col_width,
    );
}

fn render_cache_info(out: &mut String, attr: &ContextAttribution) {
    let total_input = attr.total_input_tokens;
    let cache_read = attr.cache_read_tokens;
    let cache_creation = attr.cache_creation_tokens;

    if total_input > 0 {
        let hit_ratio = (cache_read as f64 / total_input as f64) * 100.0;
        let _ = writeln!(
            out,
            "Cache hit ratio: {:.0}% (cache_read={} / total_input={})",
            hit_ratio,
            format_tokens(cache_read),
            format_tokens(total_input),
        );
    }
    if cache_creation > 0 {
        let _ = writeln!(
            out,
            "Cache creation: {} tokens",
            format_tokens(cache_creation),
        );
    }
}

fn render_top_consumers(out: &mut String, tools: &[ToolAttribution], _w: usize) {
    let _ = writeln!(out, "Top tool consumers:");
    for (i, tool) in tools.iter().take(10).enumerate() {
        let error_note = if tool.error_count > 0 {
            format!(" ({} errors)", tool.error_count)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  {}. {:<20} {} calls, {} bytes result{}",
            i + 1,
            tool.tool_name,
            tool.call_count,
            format_tokens(tool.total_result_bytes),
            error_note,
        );
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Usage;
    use crate::context_attribution::{ContextAttribution, ModelCallAttribution, ToolAttribution};

    fn sample_usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: if cache_read > 0 {
                Some(cache_read)
            } else {
                None
            },
            cache_creation_input_tokens: if cache_creation > 0 {
                Some(cache_creation)
            } else {
                None
            },
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    fn make_attribution() -> ContextAttribution {
        ContextAttribution {
            window_max: Some(200_000),
            total_input_tokens: 87_400,
            total_output_tokens: 4_800,
            cache_read_tokens: 18_400,
            cache_creation_tokens: 2_100,
            model_calls: vec![
                ModelCallAttribution {
                    model_call_id: 1,
                    message_count: 3,
                    user_message_count: 1,
                    assistant_message_count: 1,
                    tool_result_count: 1,
                    tool_count: 8,
                    token_count_estimate: None,
                    provider_kind: "anthropic_api".into(),
                    model: "claude-opus-4-7".into(),
                    renderer: Some("anthropic".into()),
                    cache_strategy: Some("sliding_window".into()),
                    span_count: Some(15),
                    cache_boundary_count: Some(2),
                    usage: Some(sample_usage(42_000, 2_400, 8_000, 1_000)),
                },
                ModelCallAttribution {
                    model_call_id: 2,
                    message_count: 7,
                    user_message_count: 2,
                    assistant_message_count: 2,
                    tool_result_count: 3,
                    tool_count: 8,
                    token_count_estimate: None,
                    provider_kind: "anthropic_api".into(),
                    model: "claude-opus-4-7".into(),
                    renderer: Some("anthropic".into()),
                    cache_strategy: Some("sliding_window".into()),
                    span_count: Some(22),
                    cache_boundary_count: Some(3),
                    usage: Some(sample_usage(87_400, 4_800, 18_400, 2_100)),
                },
            ],
            tool_attribution: vec![
                ToolAttribution {
                    tool_name: "read".into(),
                    call_count: 5,
                    total_result_bytes: 58_400,
                    error_count: 0,
                },
                ToolAttribution {
                    tool_name: "grep".into(),
                    call_count: 3,
                    total_result_bytes: 4_200,
                    error_count: 0,
                },
                ToolAttribution {
                    tool_name: "edit".into(),
                    call_count: 2,
                    total_result_bytes: 800,
                    error_count: 1,
                },
            ],
            user_message_count: 2,
            assistant_message_count: 2,
            tool_result_count: 3,
            provider_model: Some(("anthropic_api".into(), "claude-opus-4-7".into())),
        }
    }

    #[test]
    fn render_80_col_contains_key_elements() {
        let attr = make_attribution();
        let output = render(&attr, 80);

        // Header
        assert!(output.contains("Context window:"));
        assert!(output.contains("200.0K"));
        assert!(output.contains("87.4K"));

        // Region tree
        assert!(output.contains("Conversation"));
        assert!(output.contains("User messages"));
        assert!(output.contains("Tool results"));
        assert!(output.contains("Tool schemas"));

        // Cache info
        assert!(output.contains("Cache hit ratio:"));
        assert!(output.contains("cache_read=18.4K"));

        // Top consumers
        assert!(output.contains("Top tool consumers:"));
        assert!(output.contains("read"));
        assert!(output.contains("grep"));
        assert!(output.contains("edit"));
        assert!(output.contains("1 errors"));

        // Box-drawing chars
        assert!(output.contains("┌─"));
        assert!(output.contains("├─"));
        assert!(output.contains("└─"));
    }

    #[test]
    fn render_120_col_contains_key_elements() {
        let attr = make_attribution();
        let output = render(&attr, 120);

        assert!(output.contains("Context window:"));
        assert!(output.contains("Conversation"));
        assert!(output.contains("Cache hit ratio:"));
        assert!(output.contains("Top tool consumers:"));
    }

    #[test]
    fn render_80_vs_120_same_data() {
        let attr = make_attribution();
        let out80 = render(&attr, 80);
        let out120 = render(&attr, 120);

        // Both should have the same number of lines
        // (120 has wider bars but same structure)
        let lines_80 = out80.lines().count();
        let lines_120 = out120.lines().count();
        assert_eq!(lines_80, lines_120);

        // Region tree lines should have reasonable display width.
        // Use char count (not byte len) since box-drawing and bar
        // chars are multi-byte UTF-8 but single display column.
        for line in out80.lines().skip(1) {
            let display_width = line.chars().count();
            assert!(
                display_width <= 85,
                "80-col line too wide ({} chars): {}",
                display_width,
                line,
            );
        }
    }

    #[test]
    fn render_empty_attribution() {
        let attr = ContextAttribution {
            window_max: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            model_calls: Vec::new(),
            tool_attribution: Vec::new(),
            user_message_count: 0,
            assistant_message_count: 0,
            tool_result_count: 0,
            provider_model: None,
        };
        let output = render(&attr, 80);
        assert!(output.contains("Context window:"));
        // No region tree lines for empty session
        assert!(!output.contains("Conversation"));
        assert!(!output.contains("Cache hit ratio:"));
        assert!(!output.contains("Top tool consumers:"));
    }

    #[test]
    fn render_no_window_max() {
        let attr = ContextAttribution {
            window_max: None,
            total_input_tokens: 5_000,
            total_output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            model_calls: vec![ModelCallAttribution {
                model_call_id: 1,
                message_count: 2,
                user_message_count: 1,
                assistant_message_count: 1,
                tool_result_count: 0,
                tool_count: 0,
                token_count_estimate: None,
                provider_kind: "faux".into(),
                model: "test".into(),
                renderer: None,
                cache_strategy: None,
                span_count: None,
                cache_boundary_count: None,
                usage: Some(sample_usage(5_000, 500, 0, 0)),
            }],
            tool_attribution: Vec::new(),
            user_message_count: 1,
            assistant_message_count: 1,
            tool_result_count: 0,
            provider_model: None,
        };
        let output = render(&attr, 80);
        assert!(output.contains("200.0K"));
        assert!(output.contains("5000"));
    }

    #[test]
    fn render_deterministic() {
        let attr = make_attribution();
        let out1 = render(&attr, 80);
        let out2 = render(&attr, 80);
        assert_eq!(out1, out2);
    }

    #[test]
    fn format_tokens_ranges() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(9_999), "9999");
        assert_eq!(format_tokens(10_000), "10.0K");
        assert_eq!(format_tokens(87_400), "87.4K");
        assert_eq!(format_tokens(200_000), "200.0K");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn snapshot_80_col() {
        let attr = make_attribution();
        let output = render(&attr, 80);
        // Print for manual review — keep as a regression anchor.
        // Any change here should be reviewed for visual correctness.
        let line_count = output.lines().count();
        assert!(line_count >= 10, "expected >=10 lines, got {line_count}");
        // Verify structure is present in order
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines[0].contains("Context window:"));
        // Region tree comes after header
        let tree_start = lines.iter().position(|l| l.contains("┌─")).unwrap();
        let tree_end = lines.iter().position(|l| l.contains("└─")).unwrap();
        assert!(tree_start < tree_end);
    }

    #[test]
    fn snapshot_120_col() {
        let attr = make_attribution();
        let output = render(&attr, 120);
        let line_count = output.lines().count();
        assert!(line_count >= 10, "expected >=10 lines, got {line_count}");
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines[0].contains("Context window:"));
    }
}
