//! Live context tracker — monitors token usage from a claude-code
//! session's event stream and signals when compaction is warranted.
//!
//! Ported from deep-analyze.py's analyze_context_growth, made stateful
//! for live tailing.

use serde_json::Value;

/// Tracks context window fill state for a live claude-code session.
pub struct ContextTracker {
    threshold_tokens: u64,
    last_total_prompt: u64,
    peak_prompt: u64,
    turn_count: usize,
    compaction_detected: bool,
}

impl ContextTracker {
    pub fn new(threshold_tokens: u64) -> Self {
        Self {
            threshold_tokens,
            last_total_prompt: 0,
            peak_prompt: 0,
            turn_count: 0,
            compaction_detected: false,
        }
    }

    /// Feed a raw claude-code JSONL event (parsed as Value).
    /// Updates internal token accounting if the event is an assistant
    /// message with usage data.
    pub fn feed(&mut self, cc_event: &Value) {
        let cc_type = cc_event
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("");

        if cc_type != "assistant" {
            return;
        }

        let usage = match cc_event
            .get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.as_object())
        {
            Some(u) => u,
            None => return,
        };

        let input = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let total = input + cache_read + cache_creation;
        if total == 0 {
            return;
        }

        // Detect compaction: >30% drop from previous turn
        if self.last_total_prompt > 1000 && total < self.last_total_prompt * 7 / 10 {
            self.compaction_detected = true;
        }

        self.last_total_prompt = total;
        if total > self.peak_prompt {
            self.peak_prompt = total;
        }
        self.turn_count += 1;
    }

    /// Whether the session has exceeded the configured token threshold.
    pub fn should_compact(&self) -> bool {
        self.last_total_prompt >= self.threshold_tokens
    }

    /// Current total prompt tokens (last observed).
    pub fn current_tokens(&self) -> u64 {
        self.last_total_prompt
    }

    /// Peak observed prompt tokens.
    pub fn peak_tokens(&self) -> u64 {
        self.peak_prompt
    }

    /// Number of assistant turns observed with usage data.
    pub fn turn_count(&self) -> usize {
        self.turn_count
    }

    /// Fill ratio: current / threshold (0.0 to 1.0+).
    pub fn fill_ratio(&self) -> f64 {
        if self.threshold_tokens == 0 {
            return 0.0;
        }
        self.last_total_prompt as f64 / self.threshold_tokens as f64
    }

    /// Whether a compaction was detected (>30% token drop between turns).
    pub fn compaction_detected(&self) -> bool {
        self.compaction_detected
    }

    /// Reset after a `/clear` has been sent.
    pub fn reset(&mut self) {
        self.last_total_prompt = 0;
        self.peak_prompt = 0;
        self.turn_count = 0;
        self.compaction_detected = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_assistant_event(input: u64, cache_read: u64) -> Value {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "usage": {
                    "input_tokens": input,
                    "output_tokens": 100,
                    "cache_read_input_tokens": cache_read,
                    "cache_creation_input_tokens": 0,
                }
            }
        })
    }

    #[test]
    fn tracks_token_growth() {
        let mut t = ContextTracker::new(100_000);
        t.feed(&make_assistant_event(20_000, 10_000));
        assert_eq!(t.current_tokens(), 30_000);
        assert_eq!(t.turn_count(), 1);
        assert!(!t.should_compact());

        t.feed(&make_assistant_event(50_000, 60_000));
        assert_eq!(t.current_tokens(), 110_000);
        assert!(t.should_compact());
    }

    #[test]
    fn detects_compaction_drop() {
        let mut t = ContextTracker::new(500_000);
        t.feed(&make_assistant_event(100_000, 50_000)); // 150k
        assert!(!t.compaction_detected());

        t.feed(&make_assistant_event(30_000, 10_000)); // 40k — 73% drop
        assert!(t.compaction_detected());
    }

    #[test]
    fn ignores_non_assistant_events() {
        let mut t = ContextTracker::new(100_000);
        t.feed(&serde_json::json!({"type": "user", "message": {"content": "hi"}}));
        assert_eq!(t.turn_count(), 0);
        assert_eq!(t.current_tokens(), 0);
    }

    #[test]
    fn reset_clears_state() {
        let mut t = ContextTracker::new(100_000);
        t.feed(&make_assistant_event(80_000, 30_000));
        assert_eq!(t.current_tokens(), 110_000);
        t.reset();
        assert_eq!(t.current_tokens(), 0);
        assert_eq!(t.turn_count(), 0);
    }
}
