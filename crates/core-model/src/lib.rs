//! Domain model for Remote Claude Code.
//!
//! Defines the core identifiers ([`SessionId`], [`TurnId`]), state machine
//! types ([`SessionState`]), and message envelope ([`SessionMsg`]) that all
//! other crates depend on.  This crate has no external runtime dependencies.

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

/// The AI coding agent to use for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    #[default]
    ClaudeCode,
    Codex,
    Gemini,
}

impl AgentType {
    /// Determine agent type from a Slack slash command (e.g. "/cc", "/cx", "/gm").
    pub fn from_slash_command(cmd: &str) -> Self {
        match cmd {
            "/cx" => Self::Codex,
            "/gm" => Self::Gemini,
            _ => Self::ClaudeCode, // "/cc" and anything else → Claude Code
        }
    }

    /// The shell command used to launch this agent in a tmux session.
    pub fn launch_command(&self, hook_settings_path: &str) -> String {
        match self {
            Self::ClaudeCode => format!(
                "claude --settings {hook_settings_path} --dangerously-skip-permissions"
            ),
            Self::Codex => "codex".to_string(),
            Self::Gemini => "gemini".to_string(),
        }
    }

    /// Human-readable name for UI display.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini",
        }
    }
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
    /// Start or recover the session's tmux process with the given shell command.
    Recover { launch_command: String },
}
