//! Per-session token, cache, compaction, and tool execution metrics.

use uno_protocol::Usage;

use crate::protocol::pb;

#[derive(Default)]
pub(crate) struct SessionMetrics {
    pub turns: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub compactions: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
}

impl SessionMetrics {
    pub(crate) fn record_tool_call(&mut self, is_error: bool) {
        self.tool_calls += 1;
        if is_error {
            self.tool_errors += 1;
        }
    }

    pub(crate) fn record_compaction(&mut self) {
        self.compactions += 1;
    }

    pub(crate) fn record_turn(&mut self, usage: Option<&Usage>) {
        self.turns += 1;
        if let Some(u) = usage {
            self.input_tokens += u64::from(u.input_tokens);
            self.output_tokens += u64::from(u.output_tokens);
            self.cached_tokens += u64::from(u.cached_tokens.unwrap_or(0));
        }
    }

    pub(crate) fn to_response(&self) -> pb::SessionMetricsResponse {
        let cache_hit_pct = if self.input_tokens > 0 {
            (self.cached_tokens as f64 / self.input_tokens as f64) * 100.0
        } else {
            0.0
        };
        pb::SessionMetricsResponse {
            turns: self.turns,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_tokens: self.cached_tokens,
            cache_hit_pct,
            compactions: self.compactions,
            tool_calls: self.tool_calls,
            tool_errors: self.tool_errors,
        }
    }
}
