use serde::{Deserialize, Serialize};

/// Tie-break stage that produced a strict LWW winner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TieBreakStep {
    ItcCausal,
    WallTimestamp,
    AgentId,
    EventHash,
    Equal,
}

/// Structured trace payload for a single LWW merge decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeTrace {
    pub field: String,
    pub values: (String, String),
    pub winner: String,
    pub step: TieBreakStep,
    pub correlation_id: String,
    pub enabled: bool,
}

impl MergeTrace {
    pub fn disabled() -> Self {
        Self {
            field: String::new(),
            values: (String::new(), String::new()),
            winner: String::new(),
            step: TieBreakStep::Equal,
            correlation_id: String::new(),
            enabled: false,
        }
    }
}

/// Runtime toggle for merge tracing.
///
/// Enabled when either:
/// - `BONES_DEBUG_MERGE=1|true|yes|on`
/// - `BONES_LOG` contains `debug` or `trace`
pub fn merge_tracing_enabled() -> bool {
    let debug_merge = std::env::var("BONES_DEBUG_MERGE")
        .ok()
        .map(|v| {
            let lowered = v.trim().to_ascii_lowercase();
            matches!(lowered.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false);

    if debug_merge {
        return true;
    }

    std::env::var("BONES_LOG")
        .ok()
        .map(|v| {
            let lowered = v.to_ascii_lowercase();
            lowered.contains("debug") || lowered.contains("trace")
        })
        .unwrap_or(false)
}
