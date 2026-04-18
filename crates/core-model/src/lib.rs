use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectId(pub Uuid);
impl ProjectId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for ProjectId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);
impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub Uuid);
impl TurnId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransportBinding {
    pub project_space_id: String,
    pub session_space_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportStatusMessage {
    pub binding: TransportBinding,
    pub status_message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Starting,
    Idle,
    Running { active_turn: TurnId },
    WaitingForApproval,
    Cancelling { active_turn: TurnId },
    Completed,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCommand {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMsg {
    UserCommand(UserCommand),
    SendKey { key: String },
    ApprovalGranted,
    ApprovalRejected,
    RuntimeProgress { text: String },
    RuntimeCompleted { turn_id: TurnId, summary: String },
    RuntimeFailed { turn_id: TurnId, error: String },
    Interrupt,
    Terminate,
    Recover,
}
