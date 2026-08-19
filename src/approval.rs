use serde::{Deserialize, Serialize};

/// Closed outcomes for one approval request. The two allowed variants encode
/// the exact authorization scope; every other outcome is fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalOutcome {
    #[serde(rename = "allowed-once")]
    AllowedOnce,
    #[serde(rename = "allowed-for-task")]
    AllowedForTask,
    #[serde(rename = "rejected")]
    Rejected,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "unavailable")]
    Unavailable,
}

/// The minimal identity and reason needed to route and audit one approval.
/// Tool arguments stay on the already-streamed tool call and are not copied
/// into this request.
pub struct ApprovalRequest<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub tool_call_id: &'a str,
    pub approval_id: &'a str,
    pub tool_name: &'a str,
    pub reason: &'a str,
    pub high_risk: bool,
    pub allow_all: bool,
}

impl ApprovalOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllowedOnce => "allowed-once",
            Self::AllowedForTask => "allowed-for-task",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_outcomes_have_stable_wire_names() {
        assert_eq!(ApprovalOutcome::AllowedOnce.as_str(), "allowed-once");
        assert_eq!(ApprovalOutcome::AllowedForTask.as_str(), "allowed-for-task");
        assert_eq!(ApprovalOutcome::Rejected.as_str(), "rejected");
        assert_eq!(ApprovalOutcome::Cancelled.as_str(), "cancelled");
        assert_eq!(ApprovalOutcome::Unavailable.as_str(), "unavailable");
    }
}
