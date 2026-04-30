use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::diagnosis::DiagnosisReport;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchEvent {
    ToolUse {
        session_id: String,
        timestamp: String,
        tool_name: String,
        summary: String,
    },
    ToolResult {
        session_id: String,
        tool_name: String,
        outcome: String,
        duration_ms: u64,
    },
    SkillEvent {
        session_id: String,
        timestamp: String,
        skill_name: String,
        event_type: String,
        source: String,
        confidence: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    McpEvent {
        session_id: String,
        timestamp: String,
        server: String,
        tool: String,
        event_type: String,
        source: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    CacheEvent {
        session_id: String,
        event_type: String, // "hit" | "partial" | "cold_start" | "miss_ttl" | "miss_thrash"
        /// Unix-epoch seconds at which the cache TTL expires if no further
        /// request refreshes it. The orchestrator counts down from here so
        /// users see how long they have to stay warm.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_expires_at_epoch: Option<u64>,
        /// Estimated dollars it would cost to rebuild the cache from scratch
        /// if the TTL elapses — `cache_create_price × current prompt size`.
        #[serde(skip_serializing_if = "Option::is_none")]
        estimated_rebuild_cost_dollars: Option<f64>,
    },
    SessionStart {
        session_id: String,
        display_name: String,
        model: String,
        /// Cleaned excerpt of the user's first message — preamble stripped,
        /// capped to ~160 chars. `None` if the body had no usable text.
        #[serde(skip_serializing_if = "Option::is_none")]
        initial_prompt: Option<String>,
    },
    SessionEnd {
        session_id: String,
        outcome: String,
        total_tokens: u64,
        total_turns: u32,
    },
    // Note: FrustrationSignal carries a category, NOT the detected phrase text.
    // Phrase text is never stored or displayed — only the category type.
    FrustrationSignal {
        session_id: String,
        signal_type: String, // "token_pressure" | "early_stop" | "context_pressure"
    },
    CompactionLoop {
        session_id: String,
        consecutive: u32,
        wasted_tokens: u64,
    },
    Diagnosis {
        session_id: String,
        report: DiagnosisReport,
    },
    CacheWarning {
        session_id: String,
        idle_secs: u64,
        ttl_secs: u64,
    },
    /// The served model differed from the user-requested model. Anthropic
    /// fallback and Codex requested-vs-served changes both use this event.
    ModelFallback {
        session_id: String,
        requested: String,
        actual: String,
    },
    /// Codex-native per-turn accounting summary. This is intentionally separate
    /// from `CacheEvent`: Codex cached input has no Anthropic TTL/rebuild
    /// semantics.
    CodexTurnSummary {
        session_id: String,
        status: String,
        requested_model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        served_model: Option<String>,
        input_tokens: u64,
        cached_input_tokens: u64,
        uncached_input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        total_tokens: u64,
    },
    /// Per-turn context-window status. `fill_percent` follows the active
    /// provider finalization rule: copied Anthropic fallback includes cache
    /// token fields, while Codex Responses uses input tokens only. The optional
    /// `context_window_tokens` field makes the denominator explicit for clients
    /// and debugging.
    ContextStatus {
        session_id: String,
        fill_percent: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_window_tokens: Option<u64>,
        turns_to_compact: Option<u32>,
    },
    /// Latest quota snapshot for the orchestrator top strip. For Claude Code
    /// subscription traffic Anthropic does not return `anthropic-ratelimit-*`
    /// headers, so these fields are synthesized locally by the quota burn
    /// monitor from our own counters and SQLite history. Global (not
    /// session-scoped); last-writer-wins so the orchestrator can render a
    /// single top-strip meter.
    RateLimitStatus {
        /// Seconds until the current locally tracked weekly window resets
        /// (Monday 00:00 UTC).
        seconds_to_reset: Option<u64>,
        requests_remaining: Option<u64>,
        requests_limit: Option<u64>,
        input_tokens_remaining: Option<u64>,
        output_tokens_remaining: Option<u64>,
        /// Tokens used so far this week under the active or suggested cap.
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens_used_this_week: Option<u64>,
        /// Weekly token cap currently being rendered.
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens_limit: Option<u64>,
        tokens_remaining: Option<u64>,
        /// Where the current weekly cap came from: `env` or `auto_p95_4w`.
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_source: Option<String>,
        /// Projected time until the limiting dimension runs out at current
        /// burn rate (seconds). `None` when we don't have enough history.
        projected_exhaustion_secs: Option<u64>,
    },
}

pub struct EventBroadcaster {
    sender: broadcast::Sender<WatchEvent>,
    // Ring of recent events, replayed to new subscribers. Required because
    // tool_use events for a turn fire in the same finalize_response batch as
    // the SessionStart that triggers (e.g.) a tmux pane spawn — without
    // replay, the freshly-spawned pane's subscribe() races the broadcast
    // and sees nothing. Each entry carries a timestamp so we can bound
    // replay to recent events and avoid surfacing stale history to a fresh
    // live watcher.
    history: Mutex<VecDeque<(Instant, WatchEvent)>>,
}

impl EventBroadcaster {
    const HISTORY_CAP: usize = 512;
    const REPLAY_WINDOW: Duration = Duration::from_secs(30);

    fn new() -> Self {
        let (sender, _) = broadcast::channel(512);
        Self {
            sender,
            history: Mutex::new(VecDeque::with_capacity(Self::HISTORY_CAP)),
        }
    }

    /// Non-blocking broadcast. Records in history so late subscribers can replay.
    pub fn broadcast(&self, event: WatchEvent) {
        {
            let mut h = self.history.lock().unwrap();
            if h.len() >= Self::HISTORY_CAP {
                h.pop_front();
            }
            h.push_back((Instant::now(), event.clone()));
        }
        let _ = self.sender.send(event);
    }

    /// Atomic subscribe: returns a snapshot of events from the last
    /// `REPLAY_WINDOW` seconds plus a live receiver. Holding the history lock
    /// while calling `sender.subscribe()` ensures no events slip through the
    /// gap between snapshot and subscribe.
    pub fn subscribe_with_history(&self) -> (Vec<WatchEvent>, broadcast::Receiver<WatchEvent>) {
        let h = self.history.lock().unwrap();
        let rx = self.sender.subscribe();
        let cutoff = Instant::now().checked_sub(Self::REPLAY_WINDOW);
        let snap = h
            .iter()
            .filter(|(t, _)| cutoff.is_none_or(|c| *t >= c))
            .map(|(_, e)| e.clone())
            .collect();
        (snap, rx)
    }
}

/// Global event broadcaster instance.
pub static BROADCASTER: LazyLock<EventBroadcaster> = LazyLock::new(EventBroadcaster::new);

/// Extract a human-readable summary from tool input JSON.
pub fn extract_summary(tool_name: &str, tool_input_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(tool_input_json) {
        Ok(v) => v,
        Err(_) => return truncate(tool_input_json, 60),
    };

    match tool_name {
        "Read" | "Edit" | "Write" => v
            .get("file_path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string(),
        "Bash" | "bash" => truncate(v.get("command").and_then(|c| c.as_str()).unwrap_or(""), 80),
        "Glob" => v
            .get("pattern")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string(),
        "Grep" => {
            let pattern = v.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|p| p.as_str()).unwrap_or("");
            if path.is_empty() {
                pattern.to_string()
            } else {
                format!("{pattern} in {path}")
            }
        }
        "Skill" | "skill" => v
            .get("skill_name")
            .or_else(|| v.get("name"))
            .or_else(|| v.get("skill"))
            .or_else(|| v.get("command_name"))
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string(),
        _ => truncate(tool_input_json, 60),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.min(s.len())])
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_summary, EventBroadcaster, WatchEvent};

    #[test]
    fn extract_summary_renders_known_tool_inputs() {
        assert_eq!(
            extract_summary("Read", r#"{"file_path":"src/main.rs"}"#),
            "src/main.rs"
        );
        assert_eq!(
            extract_summary("Grep", r#"{"pattern":"TODO","path":"src"}"#),
            "TODO in src"
        );
        assert_eq!(extract_summary("Skill", r#"{"command_name":"tdd"}"#), "tdd");
    }

    #[test]
    fn extract_summary_truncates_large_or_unknown_payloads() {
        let long_command = format!("cargo test {}", "x".repeat(100));
        let summary = extract_summary("Bash", &format!(r#"{{"command":"{long_command}"}}"#));
        assert!(summary.starts_with("cargo test "));
        assert!(summary.ends_with("..."));
        assert!(summary.len() <= 83);

        let invalid = extract_summary("Unknown", &"x".repeat(100));
        assert!(invalid.ends_with("..."));
        assert!(invalid.len() <= 63);
    }

    #[test]
    fn broadcaster_replays_recent_history_and_live_events() {
        let broadcaster = EventBroadcaster::new();
        broadcaster.broadcast(WatchEvent::ToolUse {
            session_id: "session_a".to_string(),
            timestamp: "2999-01-01T00:00:00Z".to_string(),
            tool_name: "Read".to_string(),
            summary: "src/main.rs".to_string(),
        });

        let (history, mut rx) = broadcaster.subscribe_with_history();
        assert_eq!(history.len(), 1);
        assert!(matches!(
            &history[0],
            WatchEvent::ToolUse { session_id, tool_name, .. }
                if session_id == "session_a" && tool_name == "Read"
        ));

        broadcaster.broadcast(WatchEvent::ToolResult {
            session_id: "session_a".to_string(),
            tool_name: "Read".to_string(),
            outcome: "success".to_string(),
            duration_ms: 12,
        });
        let event = rx.try_recv().expect("live event");
        assert!(matches!(
            event,
            WatchEvent::ToolResult { outcome, duration_ms, .. }
                if outcome == "success" && duration_ms == 12
        ));
    }

    #[test]
    fn broadcaster_history_is_bounded() {
        let broadcaster = EventBroadcaster::new();
        for idx in 0..513 {
            broadcaster.broadcast(WatchEvent::ToolUse {
                session_id: format!("session_{idx}"),
                timestamp: "2999-01-01T00:00:00Z".to_string(),
                tool_name: "Read".to_string(),
                summary: "src/main.rs".to_string(),
            });
        }

        let (history, _) = broadcaster.subscribe_with_history();
        assert_eq!(history.len(), 512);
        assert!(matches!(
            &history[0],
            WatchEvent::ToolUse { session_id, .. } if session_id == "session_1"
        ));
    }

    #[test]
    fn codex_turn_summary_serializes_native_token_fields_without_cache_ttl() {
        let json = serde_json::to_value(WatchEvent::CodexTurnSummary {
            session_id: "session_codex".to_string(),
            status: "completed".to_string(),
            requested_model: "gpt-codex-fixture".to_string(),
            served_model: Some("gpt-codex-served".to_string()),
            input_tokens: 1280,
            cached_input_tokens: 512,
            uncached_input_tokens: 768,
            output_tokens: 96,
            reasoning_output_tokens: 32,
            total_tokens: 1376,
        })
        .expect("serialize codex turn summary");

        assert_eq!(
            json.get("type").and_then(|v| v.as_str()),
            Some("codex_turn_summary")
        );
        assert_eq!(
            json.get("cached_input_tokens").and_then(|v| v.as_u64()),
            Some(512)
        );
        assert_eq!(
            json.get("reasoning_output_tokens").and_then(|v| v.as_u64()),
            Some(32)
        );
        assert!(json.get("cache_expires_at_epoch").is_none());
        assert!(json.get("estimated_rebuild_cost_dollars").is_none());
    }
}
