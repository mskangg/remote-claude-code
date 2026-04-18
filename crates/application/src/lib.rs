//! Slack application use-cases and orchestration.
//!
//! [`SlackApplicationService`] implements [`SlackSessionOrchestrator`]: it
//! handles new-session creation, thread-reply routing, session listing, and
//! thread-action dispatch.  [`SlackSessionLifecycleObserver`] translates
//! runtime events into Slack status messages and final replies.  All Slack UX
//! rules (status message lifecycle, command palette blocks, etc.) live here.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use core_model::{SessionId, SessionMsg, SessionState, TransportBinding};
use core_service::{SessionRuntimeLiveness, SessionStateObserver};
use slack_morphism::prelude::*;
use thiserror::Error;
use transport_slack::{
    SessionBindingStore, SessionStatusStore, SlackListedSession, SlackMessageTarget,
    SlackPostedMessage, SlackProject, SlackProjectLocator, SlackSessionCatalogStore,
    SlackSessionOrchestrator, SlackSessionPublisher, SlackSessionStart, SlackThreadAction,
    SlackThreadReply, SlackThreadStatus, SlackTransport, SlackWorkingStatusPublisher,
    StartedSlackSession,
};
use url::Url;

const INITIAL_THINKING_STATUS: &str = "⏳ Working...";

#[derive(Debug, Error)]
pub enum ApplicationError {
    #[error("no project mapping configured for Slack channel {channel_id}")]
    NoProjectMapping { channel_id: String },
    #[error("invalid Slack permalink: {0}")]
    InvalidPermalink(#[from] url::ParseError),
    #[error(transparent)]
    Infrastructure(#[from] anyhow::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackSessionListEntry {
    pub thread_ts: String,
    pub project_label: String,
    pub tmux_session_name: String,
    pub permalink: Url,
    pub state: SessionState,
}

pub struct SlackApplicationService<S, R, C, L, P> {
    transport: Arc<SlackTransport<S, R, C>>,
    project_locator: Arc<L>,
    publisher: Arc<P>,
}

impl<S, R, C, L, P> SlackApplicationService<S, R, C, L, P>
where
    S: SessionBindingStore
        + SessionStatusStore
        + transport_slack::SessionStatusRegistrar
        + SlackSessionCatalogStore,
    R: transport_slack::SessionHandleResolver,
    C: core_service::SessionRuntimeConfigurator + SessionRuntimeLiveness,
    L: SlackProjectLocator,
    P: SlackWorkingStatusPublisher + SlackSessionPublisher,
{
    pub fn new(transport: Arc<SlackTransport<S, R, C>>, project_locator: Arc<L>, publisher: Arc<P>) -> Self {
        Self {
            transport,
            project_locator,
            publisher,
        }
    }

    async fn post_session_start_message(
        &self,
        channel_id: &str,
        project: &SlackProject,
    ) -> Result<SlackPostedMessage, ApplicationError> {
        self.publisher
            .post_channel_message(
                channel_id,
                &format!(
                    "Remote Claude Code session started for {}. Reply in this thread to continue.",
                    project.project_label
                ),
            )
            .await
            .map_err(ApplicationError::from)
    }

    async fn post_session_controls(
        &self,
        binding: &TransportBinding,
    ) -> Result<(), ApplicationError> {
        let target = SlackMessageTarget {
            channel_id: binding.project_space_id.clone(),
            thread_ts: binding.session_space_id.clone(),
        };
        self.publisher
            .post_thread_message_with_blocks(&target, "Session controls", build_session_control_blocks())
            .await?;
        Ok(())
    }

    async fn post_command_palette(
        &self,
        channel_id: &str,
        thread_ts: &str,
    ) -> Result<(), ApplicationError> {
        let target = SlackMessageTarget {
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts.to_string(),
        };
        self.publisher
            .post_thread_message_with_blocks(&target, "Session controls", build_command_palette_blocks())
            .await?;
        Ok(())
    }

    async fn sync_status_for_state(
        &self,
        binding: &TransportBinding,
        state: &SessionState,
    ) -> Result<(), ApplicationError> {
        match state {
            SessionState::Starting | SessionState::Running { .. } | SessionState::Cancelling { .. } => {
                self.transport
                    .ensure_working_status(binding, self.publisher.as_ref(), INITIAL_THINKING_STATUS)
                    .await?;
            }
            _ => {}
        }

        Ok(())
    }

    async fn list_channel_sessions_internal(
        &self,
        channel_id: &str,
    ) -> Result<Vec<SlackListedSession>, ApplicationError> {
        let project_label = self
            .project_locator
            .find_project(channel_id)
            .await?
            .map(|project| project.project_label)
            .unwrap_or_else(|| "project".to_string());
        let mut sessions = self.transport.list_channel_sessions(channel_id).await?;
        let mut live_sessions = Vec::with_capacity(sessions.len());
        for mut session in sessions.drain(..) {
            if !self.transport.configurator().is_session_alive(session.session_id).await? {
                continue;
            }
            if session.project_label.is_empty() {
                session.project_label = project_label.clone();
            }
            live_sessions.push(session);
        }
        Ok(live_sessions)
    }

    pub async fn start_new_session_internal(
        &self,
        channel_id: &str,
    ) -> Result<StartedSlackSession, ApplicationError>
    where
        S: transport_slack::SessionBindingRegistrar + transport_slack::SessionStatusRegistrar,
    {
        let project = self
            .project_locator
            .find_project(channel_id)
            .await?
            .ok_or_else(|| ApplicationError::NoProjectMapping {
                channel_id: channel_id.to_string(),
            })?;
        let post = self.post_session_start_message(channel_id, &project).await?;
        let started = self
            .transport
            .start_session(
                SlackSessionStart {
                    channel_id: post.channel_id,
                    thread_ts: post.message_ts,
                },
                &project.project_root,
            )
            .await?;
        self.post_session_controls(&started.binding).await?;
        Ok(started)
    }

    pub async fn handle_session_reply_internal(
        &self,
        reply: SlackThreadReply,
    ) -> Result<SessionState, ApplicationError> {
        let binding = TransportBinding {
            project_space_id: reply.channel_id.clone(),
            session_space_id: reply.thread_ts.clone(),
        };
        let state = self.transport.handle_thread_reply(reply).await?;
        self.sync_status_for_state(&binding, &state).await?;
        Ok(state)
    }

    pub async fn post_session_list_internal(
        &self,
        channel_id: &str,
        thread_ts: &str,
    ) -> Result<(), ApplicationError> {
        let sessions = self.list_channel_sessions_internal(channel_id).await?;
        let mut entries = Vec::with_capacity(sessions.len());
        for session in sessions {
            let permalink = self
                .publisher
                .get_message_permalink(channel_id, &session.thread_ts)
                .await?;
            entries.push(SlackSessionListEntry {
                thread_ts: session.thread_ts,
                project_label: session.project_label,
                tmux_session_name: session.tmux_session_name,
                permalink: Url::parse(&permalink)?,
                state: session.state,
            });
        }
        let target = SlackMessageTarget {
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts.to_string(),
        };
        self.publisher
            .post_thread_message_with_blocks(
                &target,
                &build_session_list_response_text(&entries),
                build_session_list_blocks(&entries),
            )
            .await?;
        Ok(())
    }

    pub async fn handle_thread_action_internal(
        &self,
        channel_id: &str,
        thread_ts: &str,
        action: SlackThreadAction,
    ) -> Result<Option<SessionState>, ApplicationError> {
        match action {
            SlackThreadAction::OpenCommandPalette => {
                self.transport
                    .handle_thread_action(channel_id, thread_ts, SlackThreadAction::OpenCommandPalette)
                    .await?;
                self.post_command_palette(channel_id, thread_ts).await?;
                Ok(None)
            }
            SlackThreadAction::Interrupt => {
                let state = self
                    .transport
                    .handle_thread_action(channel_id, thread_ts, SlackThreadAction::Interrupt)
                    .await?;
                let _ = self
                    .transport
                    .handle_thread_action(
                        channel_id,
                        thread_ts,
                        SlackThreadAction::SendKey {
                            key: "C-u".to_string(),
                        },
                    )
                    .await?;
                let binding = TransportBinding {
                    project_space_id: channel_id.to_string(),
                    session_space_id: thread_ts.to_string(),
                };
                self.sync_status_for_state(&binding, &state).await?;
                Ok(Some(state))
            }
            SlackThreadAction::SendKey { key } => {
                let state = self
                    .transport
                    .handle_thread_action(channel_id, thread_ts, SlackThreadAction::SendKey { key })
                    .await?;
                Ok(Some(state))
            }
            SlackThreadAction::SendCommand { text } => {
                let state = self
                    .transport
                    .handle_thread_action(
                        channel_id,
                        thread_ts,
                        SlackThreadAction::SendCommand { text },
                    )
                    .await?;
                let binding = TransportBinding {
                    project_space_id: channel_id.to_string(),
                    session_space_id: thread_ts.to_string(),
                };
                self.sync_status_for_state(&binding, &state).await?;
                Ok(Some(state))
            }
            SlackThreadAction::Terminate => {
                let state = self
                    .transport
                    .handle_thread_action(channel_id, thread_ts, SlackThreadAction::Terminate)
                    .await?;
                let binding = TransportBinding {
                    project_space_id: channel_id.to_string(),
                    session_space_id: thread_ts.to_string(),
                };
                self.sync_status_for_state(&binding, &state).await?;
                self.transport
                    .post_final_reply(&binding, self.publisher.as_ref(), "Session terminated.")
                    .await?;
                Ok(Some(state))
            }
        }
    }
}

#[async_trait]
impl<S, R, C, L, P> SlackSessionOrchestrator for SlackApplicationService<S, R, C, L, P>
where
    S: SessionBindingStore
        + transport_slack::SessionBindingRegistrar
        + SessionStatusStore
        + transport_slack::SessionStatusRegistrar
        + SlackSessionCatalogStore
        + Send
        + Sync
        + 'static,
    R: transport_slack::SessionHandleResolver + Send + Sync + 'static,
    C: core_service::SessionRuntimeConfigurator + SessionRuntimeLiveness + Send + Sync + 'static,
    L: SlackProjectLocator + Send + Sync + 'static,
    P: SlackWorkingStatusPublisher + SlackSessionPublisher + Send + Sync + 'static,
{
    async fn start_new_session(&self, channel_id: &str) -> Result<StartedSlackSession> {
        self.start_new_session_internal(channel_id)
            .await
            .map_err(Into::into)
    }

    async fn handle_session_reply(&self, reply: SlackThreadReply) -> Result<SessionState> {
        self.handle_session_reply_internal(reply)
            .await
            .map_err(Into::into)
    }

    async fn list_channel_sessions(&self, channel_id: &str) -> Result<Vec<SlackListedSession>> {
        self.list_channel_sessions_internal(channel_id)
            .await
            .map_err(Into::into)
    }

    async fn post_session_list(&self, channel_id: &str, thread_ts: &str) -> Result<()> {
        self.post_session_list_internal(channel_id, thread_ts)
            .await
            .map_err(Into::into)
    }

    async fn handle_thread_action(
        &self,
        channel_id: &str,
        thread_ts: &str,
        action: SlackThreadAction,
    ) -> Result<Option<SessionState>> {
        self.handle_thread_action_internal(channel_id, thread_ts, action)
            .await
            .map_err(Into::into)
    }
}

pub struct SlackSessionLifecycleObserver<S, P> {
    store: Arc<S>,
    publisher: Arc<P>,
}

impl<S, P> SlackSessionLifecycleObserver<S, P> {
    pub fn new(store: Arc<S>, publisher: Arc<P>) -> Self {
        Self { store, publisher }
    }
}

#[async_trait]
impl<S, P> SessionStateObserver for SlackSessionLifecycleObserver<S, P>
where
    S: SessionBindingStore + SessionStatusStore + Send + Sync + 'static,
    P: SlackSessionPublisher + Send + Sync + 'static,
{
    async fn on_state_changed(
        &self,
        session_id: SessionId,
        message: &SessionMsg,
        next_state: &SessionState,
    ) -> Result<()> {
        let Some(binding) = self.store.find_binding(session_id).await? else {
            return Ok(());
        };

        let Some(status) = self.store.find_status_message(&binding).await? else {
            return Ok(());
        };

        let status = SlackThreadStatus {
            channel_id: binding.project_space_id.clone(),
            thread_ts: binding.session_space_id.clone(),
            status_message_ts: status.status_message_id,
        };
        let target = SlackMessageTarget {
            channel_id: binding.project_space_id,
            thread_ts: binding.session_space_id,
        };

        match (message, next_state) {
            (SessionMsg::RuntimeProgress { text }, SessionState::Running { .. })
            | (SessionMsg::RuntimeProgress { text }, SessionState::Cancelling { .. }) => {
                self.publisher
                    .update_working_status(&status, &render_progress_status_text(text))
                    .await?;
            }
            (SessionMsg::RuntimeCompleted { summary, .. }, SessionState::Idle) => {
                self.publisher.delete_message(&status).await?;
                self.publisher.post_final_reply(&target, summary).await?;
            }
            (SessionMsg::RuntimeFailed { error, .. }, SessionState::Failed { .. }) => {
                self.publisher.delete_message(&status).await?;
                self.publisher
                    .post_final_reply(&target, &format!("Session failed: {error}"))
                    .await?;
            }
            _ => {}
        }

        Ok(())
    }
}

fn render_progress_status_text(tool_name: &str) -> String {
    let normalized = tool_name.to_lowercase();

    if normalized == "done" {
        return "✅ Finalizing response...".to_string();
    }

    if normalized.contains("grep")
        || normalized.contains("glob")
        || normalized.contains("search")
        || normalized.contains("qmd")
    {
        return "🔎 Searching...".to_string();
    }

    if normalized.contains("read") || normalized.contains("multi_get") {
        return "📄 Reading files...".to_string();
    }

    if normalized.contains("edit")
        || normalized.contains("write")
        || normalized.contains("notebookedit")
    {
        return "✏️ Editing files...".to_string();
    }

    if normalized.contains("bash") {
        return "🛠 Running commands...".to_string();
    }

    match normalized
        .bytes()
        .fold(0usize, |acc, byte| acc.wrapping_add(byte as usize))
        % 3
    {
        0 => "⏳ Working...".to_string(),
        1 => "⌛ Working...".to_string(),
        _ => "🔄 Working...".to_string(),
    }
}

fn build_session_control_blocks() -> Vec<SlackBlock> {
    vec![SlackActionsBlock {
        block_id: None,
        elements: vec![SlackBlockButtonElement {
            action_id: SlackActionId("claude_command_palette_open".to_string()),
            text: SlackBlockPlainText::new("Commands".to_string()).into(),
            url: None,
            value: Some("open".to_string()),
            style: None,
            confirm: None,
        }
        .into()],
    }
    .into()]
}

fn build_command_palette_blocks() -> Vec<SlackBlock> {
    vec![
        SlackSectionBlock {
            block_id: None,
            text: Some(SlackBlockMarkDownText::new("Session controls".to_string()).into()),
            fields: None,
            accessory: None,
        }
        .into(),
        SlackActionsBlock {
            block_id: None,
            elements: vec![
                SlackBlockButtonElement {
                    action_id: SlackActionId("claude_command_key_interrupt".to_string()),
                    text: SlackBlockPlainText::new("Interrupt".to_string()).into(),
                    url: None,
                    value: Some("C-c".to_string()),
                    style: Some("danger".to_string()),
                    confirm: None,
                }
                .into(),
                SlackBlockButtonElement {
                    action_id: SlackActionId("claude_terminal_key_escape".to_string()),
                    text: SlackBlockPlainText::new("Esc".to_string()).into(),
                    url: None,
                    value: Some("Escape".to_string()),
                    style: None,
                    confirm: None,
                }
                .into(),
                SlackBlockButtonElement {
                    action_id: SlackActionId("claude_command_send_clear".to_string()),
                    text: SlackBlockPlainText::new("Clear".to_string()).into(),
                    url: None,
                    value: Some("/clear".to_string()),
                    style: None,
                    confirm: None,
                }
                .into(),
                SlackBlockButtonElement {
                    action_id: SlackActionId("claude_command_send_revise_claude_md".to_string()),
                    text: SlackBlockPlainText::new("CLAUDE.md update".to_string()).into(),
                    url: None,
                    value: Some("/claude-md-management:revise-claude-md".to_string()),
                    style: None,
                    confirm: None,
                }
                .into(),
                SlackBlockButtonElement {
                    action_id: SlackActionId("claude_session_terminate".to_string()),
                    text: SlackBlockPlainText::new("Terminate session".to_string()).into(),
                    url: None,
                    value: Some("terminate".to_string()),
                    style: Some("danger".to_string()),
                    confirm: None,
                }
                .into(),
            ],
        }
        .into(),
    ]
}

fn render_state_label(state: &SessionState) -> &'static str {
    match state {
        SessionState::Idle => "Ready for next prompt.",
        SessionState::Starting | SessionState::Running { .. } | SessionState::Cancelling { .. } => {
            INITIAL_THINKING_STATUS
        }
        SessionState::Completed => "Completed.",
        SessionState::Failed { .. } => "Failed.",
        SessionState::WaitingForApproval => "Waiting for approval.",
    }
}

fn build_session_list_response_text(entries: &[SlackSessionListEntry]) -> String {
    if entries.is_empty() {
        return "No active sessions.".to_string();
    }

    let lines = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            format!(
                "{}. {} · {} · {}",
                index + 1,
                entry.project_label,
                entry.thread_ts,
                render_state_label(&entry.state)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!("Active sessions:\n{lines}")
}

fn build_session_list_blocks(sessions: &[SlackSessionListEntry]) -> Vec<SlackBlock> {
    if sessions.is_empty() {
        return vec![SlackSectionBlock {
            block_id: None,
            text: Some(SlackBlockMarkDownText::new("No active sessions.".to_string()).into()),
            fields: None,
            accessory: None,
        }
        .into()];
    }

    let mut blocks = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        blocks.push(
            SlackSectionBlock {
                block_id: None,
                text: Some(
                    SlackBlockMarkDownText::new(format!(
                        "*{}*\nSession: `{}`\nThread: `{}`",
                        session.project_label, session.tmux_session_name, session.thread_ts
                    ))
                    .into(),
                ),
                fields: None,
                accessory: None,
            }
            .into(),
        );
        blocks.push(
            SlackActionsBlock {
                block_id: None,
                elements: vec![SlackBlockButtonElement {
                    action_id: SlackActionId("claude_session_open_thread".to_string()),
                    text: SlackBlockPlainText::new("Open thread".to_string()).into(),
                    url: Some(session.permalink.clone()),
                    value: None,
                    style: None,
                    confirm: None,
                }
                .into()],
            }
            .into(),
        );
        if index + 1 < sessions.len() {
            blocks.push(SlackDividerBlock { block_id: None }.into());
        }
    }

    blocks
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use core_model::{TransportStatusMessage, TurnId};
    use core_service::{
        SessionHandle, SessionRepository, SessionRequest, SessionRuntimeConfigurator,
        SessionRuntimeLiveness,
    };
    use session_store::SqliteSessionRepository;
    use tempfile::tempdir;
    use tokio::sync::{mpsc, Mutex};
    use transport_slack::{
        InMemorySlackBindingStore, SessionBindingRegistrar, SessionHandleResolver, SessionStatusRegistrar,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingConfigurator {
        registrations: Mutex<Vec<(SessionId, String)>>,
        live_sessions: Mutex<HashMap<SessionId, bool>>,
    }

    #[async_trait]
    impl SessionRuntimeConfigurator for RecordingConfigurator {
        async fn register_project_root(
            &self,
            session_id: SessionId,
            project_root: &str,
        ) -> Result<()> {
            self.registrations
                .lock()
                .await
                .push((session_id, project_root.to_string()));
            Ok(())
        }
    }

    #[async_trait]
    impl SessionRuntimeLiveness for RecordingConfigurator {
        async fn is_session_alive(&self, session_id: SessionId) -> Result<bool> {
            Ok(self
                .live_sessions
                .lock()
                .await
                .get(&session_id)
                .copied()
                .unwrap_or(true))
        }
    }

    #[derive(Default)]
    struct RecordingProjectLocator {
        projects: Mutex<HashMap<String, SlackProject>>,
    }

    impl RecordingProjectLocator {
        async fn insert(&self, channel_id: &str, project: SlackProject) {
            self.projects
                .lock()
                .await
                .insert(channel_id.to_string(), project);
        }
    }

    #[async_trait]
    impl SlackProjectLocator for RecordingProjectLocator {
        async fn find_project(&self, channel_id: &str) -> Result<Option<SlackProject>> {
            Ok(self.projects.lock().await.get(channel_id).cloned())
        }
    }

    #[derive(Default)]
    struct RecordingResolver {
        handle: Option<SessionHandle>,
    }

    #[async_trait]
    impl SessionHandleResolver for RecordingResolver {
        async fn resolve(&self, _session_id: SessionId) -> Result<SessionHandle> {
            self.handle
                .clone()
                .ok_or_else(|| anyhow::anyhow!("missing session handle"))
        }
    }

    fn fake_handle(session_id: SessionId, state: SessionState) -> SessionHandle {
        let (tx, mut rx) = mpsc::channel::<SessionRequest>(4);
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let _ = request.reply_tx.send(Ok(state.clone()));
            }
        });
        SessionHandle::new_for_tests(session_id, tx)
    }

    #[derive(Default)]
    struct RecordingSessionPublisher {
        channel_messages: Mutex<Vec<(String, String)>>,
        threaded_block_messages: Mutex<Vec<(SlackMessageTarget, String, usize)>>,
        status_updates: Mutex<Vec<(SlackThreadStatus, String)>>,
        deleted_messages: Mutex<Vec<SlackThreadStatus>>,
        final_replies: Mutex<Vec<(SlackMessageTarget, String)>>,
        permalink_requests: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl SlackSessionPublisher for RecordingSessionPublisher {
        async fn post_channel_message(&self, channel_id: &str, text: &str) -> Result<SlackPostedMessage> {
            self.channel_messages
                .lock()
                .await
                .push((channel_id.to_string(), text.to_string()));
            Ok(SlackPostedMessage {
                channel_id: channel_id.to_string(),
                message_ts: "1740.100".to_string(),
            })
        }

        async fn post_thread_message_with_blocks(
            &self,
            target: &SlackMessageTarget,
            text: &str,
            blocks: Vec<SlackBlock>,
        ) -> Result<SlackPostedMessage> {
            self.threaded_block_messages
                .lock()
                .await
                .push((target.clone(), text.to_string(), blocks.len()));
            Ok(SlackPostedMessage {
                channel_id: target.channel_id.clone(),
                message_ts: "1740.300".to_string(),
            })
        }

        async fn update_working_status(&self, status: &SlackThreadStatus, text: &str) -> Result<()> {
            self.status_updates
                .lock()
                .await
                .push((status.clone(), text.to_string()));
            Ok(())
        }

        async fn delete_message(&self, status: &SlackThreadStatus) -> Result<()> {
            self.deleted_messages.lock().await.push(status.clone());
            Ok(())
        }

        async fn get_message_permalink(&self, channel_id: &str, message_ts: &str) -> Result<String> {
            self.permalink_requests
                .lock()
                .await
                .push((channel_id.to_string(), message_ts.to_string()));
            Ok(format!("https://example.com/{channel_id}/{message_ts}"))
        }

        async fn post_final_reply(
            &self,
            target: &SlackMessageTarget,
            text: &str,
        ) -> Result<SlackPostedMessage> {
            self.final_replies
                .lock()
                .await
                .push((target.clone(), text.to_string()));
            Ok(SlackPostedMessage {
                channel_id: target.channel_id.clone(),
                message_ts: "1740.400".to_string(),
            })
        }
    }

    #[async_trait]
    impl SlackWorkingStatusPublisher for RecordingSessionPublisher {
        async fn post_working_status(
            &self,
            target: &SlackMessageTarget,
            text: impl Into<String> + Send,
        ) -> Result<SlackThreadStatus> {
            let text = text.into();
            self.status_updates.lock().await.push((
                SlackThreadStatus {
                    channel_id: target.channel_id.clone(),
                    thread_ts: target.thread_ts.clone(),
                    status_message_ts: "1740.200".to_string(),
                },
                text,
            ));
            Ok(SlackThreadStatus {
                channel_id: target.channel_id.clone(),
                thread_ts: target.thread_ts.clone(),
                status_message_ts: "1740.200".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn application_service_starts_thread_without_posting_working_status() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let resolver = Arc::new(RecordingResolver {
            handle: Some(fake_handle(SessionId::new(), SessionState::Idle)),
        });
        let transport = Arc::new(SlackTransport::new(
            store.clone(),
            resolver,
            Arc::new(RecordingConfigurator::default()),
        ));
        let publisher = Arc::new(RecordingSessionPublisher::default());
        let locator = Arc::new(RecordingProjectLocator::default());
        locator
            .insert(
                "C777",
                SlackProject {
                    project_root: "/tmp/project".to_string(),
                    project_label: "demo".to_string(),
                },
            )
            .await;
        let service = SlackApplicationService::new(transport, locator, publisher.clone());

        let started = service
            .start_new_session_internal("C777")
            .await
            .expect("start new session");

        assert_eq!(started.binding.project_space_id, "C777");
        assert_eq!(started.binding.session_space_id, "1740.100");
        assert!(
            store.find_status_message(&started.binding)
                .await
                .expect("find status")
                .is_none()
        );
        assert_eq!(publisher.channel_messages.lock().await.len(), 1);
        assert!(publisher.status_updates.lock().await.is_empty());
        assert_eq!(publisher.threaded_block_messages.lock().await.as_slice(), &[(
            SlackMessageTarget {
                channel_id: "C777".to_string(),
                thread_ts: "1740.100".to_string(),
            },
            "Session controls".to_string(),
            1,
        )]);
    }

    #[tokio::test]
    async fn application_service_posts_first_working_status_when_thread_reply_starts_running_turn() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let session_id = SessionId::new();
        let binding = TransportBinding {
            project_space_id: "C777".to_string(),
            session_space_id: "1740.100".to_string(),
        };
        store
            .save_binding(&binding, session_id)
            .await
            .expect("save binding");
        let resolver = Arc::new(RecordingResolver {
            handle: Some(fake_handle(
                session_id,
                SessionState::Running {
                    active_turn: TurnId::new(),
                },
            )),
        });
        let transport = Arc::new(SlackTransport::new(
            store.clone(),
            resolver,
            Arc::new(RecordingConfigurator::default()),
        ));
        let publisher = Arc::new(RecordingSessionPublisher::default());
        let locator = Arc::new(RecordingProjectLocator::default());
        let service = SlackApplicationService::new(transport, locator, publisher.clone());

        let state = service
            .handle_session_reply_internal(SlackThreadReply {
                channel_id: "C777".to_string(),
                thread_ts: "1740.100".to_string(),
                text: "ㅎㅇ".to_string(),
            })
            .await
            .expect("handle reply");

        assert!(matches!(state, SessionState::Running { .. }));
        let persisted = store
            .find_status_message(&binding)
            .await
            .expect("find status")
            .expect("status should exist after first reply");
        assert_eq!(persisted.status_message_id, "1740.200");
        assert_eq!(publisher.status_updates.lock().await.as_slice(), &[(
            SlackThreadStatus {
                channel_id: "C777".to_string(),
                thread_ts: "1740.100".to_string(),
                status_message_ts: "1740.200".to_string(),
            },
            "⏳ Working...".to_string(),
        )]);
    }

    #[tokio::test]
    async fn application_service_posts_session_list_with_permalink_buttons() {
        let temp_dir = tempdir().expect("create temp dir");
        let store = Arc::new(
            SqliteSessionRepository::new(temp_dir.path().join("state.db"))
                .expect("create sqlite repository"),
        );
        let first_session_id = SessionId::new();
        let second_session_id = SessionId::new();
        store
            .save_state(first_session_id, &SessionState::Idle)
            .await
            .expect("save first state");
        store
            .save_state(second_session_id, &SessionState::Idle)
            .await
            .expect("save second state");
        store
            .save_binding(
                &TransportBinding {
                    project_space_id: "C777".to_string(),
                    session_space_id: "3000.100".to_string(),
                },
                first_session_id,
            )
            .await
            .expect("save first binding");
        store
            .save_binding(
                &TransportBinding {
                    project_space_id: "C777".to_string(),
                    session_space_id: "3000.200".to_string(),
                },
                second_session_id,
            )
            .await
            .expect("save second binding");
        let transport = Arc::new(SlackTransport::new(
            store,
            Arc::new(RecordingResolver::default()),
            Arc::new(RecordingConfigurator::default()),
        ));
        let publisher = Arc::new(RecordingSessionPublisher::default());
        let locator = Arc::new(RecordingProjectLocator::default());
        locator
            .insert(
                "C777",
                SlackProject {
                    project_root: "/tmp/project".to_string(),
                    project_label: "economics-education".to_string(),
                },
            )
            .await;
        let service = SlackApplicationService::new(transport, locator, publisher.clone());

        service
            .post_session_list_internal("C777", "1740.900")
            .await
            .expect("post session list");

        assert_eq!(publisher.permalink_requests.lock().await.len(), 2);
        assert_eq!(publisher.threaded_block_messages.lock().await.as_slice(), &[(
            SlackMessageTarget {
                channel_id: "C777".to_string(),
                thread_ts: "1740.900".to_string(),
            },
            "Active sessions:\n1. economics-education · 3000.200 · Ready for next prompt.\n2. economics-education · 3000.100 · Ready for next prompt.".to_string(),
            5,
        )]);
    }

    #[tokio::test]
    async fn application_service_omits_sessions_without_live_tmux_session() {
        let temp_dir = tempdir().expect("create temp dir");
        let store = Arc::new(
            SqliteSessionRepository::new(temp_dir.path().join("state.db"))
                .expect("create sqlite repository"),
        );
        let stale_session_id = SessionId::new();
        store
            .save_state(stale_session_id, &SessionState::Idle)
            .await
            .expect("save stale state");
        store
            .save_binding(
                &TransportBinding {
                    project_space_id: "C777".to_string(),
                    session_space_id: "3000.100".to_string(),
                },
                stale_session_id,
            )
            .await
            .expect("save stale binding");
        let configurator = Arc::new(RecordingConfigurator::default());
        configurator
            .live_sessions
            .lock()
            .await
            .insert(stale_session_id, false);
        let transport = Arc::new(SlackTransport::new(
            store,
            Arc::new(RecordingResolver::default()),
            configurator,
        ));
        let publisher = Arc::new(RecordingSessionPublisher::default());
        let locator = Arc::new(RecordingProjectLocator::default());
        locator
            .insert(
                "C777",
                SlackProject {
                    project_root: "/tmp/project".to_string(),
                    project_label: "economics-education".to_string(),
                },
            )
            .await;
        let service = SlackApplicationService::new(transport, locator, publisher.clone());

        service
            .post_session_list_internal("C777", "1740.900")
            .await
            .expect("post session list");

        assert!(publisher.permalink_requests.lock().await.is_empty());
        assert_eq!(publisher.threaded_block_messages.lock().await.as_slice(), &[(
            SlackMessageTarget {
                channel_id: "C777".to_string(),
                thread_ts: "1740.900".to_string(),
            },
            "No active sessions.".to_string(),
            1,
        )]);
    }

    #[tokio::test]
    async fn lifecycle_observer_posts_final_reply_on_runtime_completed() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let session_id = SessionId::new();
        let binding = TransportBinding {
            project_space_id: "C777".to_string(),
            session_space_id: "3000.100".to_string(),
        };
        store.insert(binding.clone(), session_id).await;
        store
            .save_status_message(&TransportStatusMessage {
                binding: binding.clone(),
                status_message_id: "3000.200".to_string(),
            })
            .await
            .expect("save status");
        let publisher = Arc::new(RecordingSessionPublisher::default());
        let observer = SlackSessionLifecycleObserver::new(store, publisher.clone());

        observer
            .on_state_changed(
                session_id,
                &SessionMsg::RuntimeCompleted {
                    turn_id: TurnId::new(),
                    summary: "done".to_string(),
                },
                &SessionState::Idle,
            )
            .await
            .expect("observe completion");

        assert!(publisher.status_updates.lock().await.is_empty());
        assert_eq!(publisher.deleted_messages.lock().await.as_slice(), &[SlackThreadStatus {
            channel_id: "C777".to_string(),
            thread_ts: "3000.100".to_string(),
            status_message_ts: "3000.200".to_string(),
        }]);
        assert_eq!(publisher.final_replies.lock().await.as_slice(), &[(
            SlackMessageTarget {
                channel_id: "C777".to_string(),
                thread_ts: "3000.100".to_string(),
            },
            "done".to_string(),
        )]);
    }

    #[tokio::test]
    async fn lifecycle_observer_deletes_status_before_failure_reply() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let session_id = SessionId::new();
        let binding = TransportBinding {
            project_space_id: "C777".to_string(),
            session_space_id: "3000.100".to_string(),
        };
        store.insert(binding.clone(), session_id).await;
        store
            .save_status_message(&TransportStatusMessage {
                binding: binding.clone(),
                status_message_id: "3000.200".to_string(),
            })
            .await
            .expect("save status");
        let publisher = Arc::new(RecordingSessionPublisher::default());
        let observer = SlackSessionLifecycleObserver::new(store, publisher.clone());

        observer
            .on_state_changed(
                session_id,
                &SessionMsg::RuntimeFailed {
                    turn_id: TurnId::new(),
                    error: "boom".to_string(),
                },
                &SessionState::Failed {
                    reason: "boom".to_string(),
                },
            )
            .await
            .expect("observe failure");

        assert!(publisher.status_updates.lock().await.is_empty());
        assert_eq!(publisher.deleted_messages.lock().await.as_slice(), &[SlackThreadStatus {
            channel_id: "C777".to_string(),
            thread_ts: "3000.100".to_string(),
            status_message_ts: "3000.200".to_string(),
        }]);
        assert_eq!(publisher.final_replies.lock().await.as_slice(), &[(
            SlackMessageTarget {
                channel_id: "C777".to_string(),
                thread_ts: "3000.100".to_string(),
            },
            "Session failed: boom".to_string(),
        )]);
    }

    #[tokio::test]
    async fn lifecycle_observer_updates_status_for_runtime_progress() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let session_id = SessionId::new();
        let binding = TransportBinding {
            project_space_id: "C777".to_string(),
            session_space_id: "3000.100".to_string(),
        };
        store.insert(binding.clone(), session_id).await;
        store
            .save_status_message(&TransportStatusMessage {
                binding: binding.clone(),
                status_message_id: "3000.200".to_string(),
            })
            .await
            .expect("save status");
        let publisher = Arc::new(RecordingSessionPublisher::default());
        let observer = SlackSessionLifecycleObserver::new(store, publisher.clone());

        observer
            .on_state_changed(
                session_id,
                &SessionMsg::RuntimeProgress {
                    text: "done".to_string(),
                },
                &SessionState::Running {
                    active_turn: core_model::TurnId::new(),
                },
            )
            .await
            .expect("observe progress");

        assert_eq!(publisher.status_updates.lock().await.as_slice(), &[(
            SlackThreadStatus {
                channel_id: "C777".to_string(),
                thread_ts: "3000.100".to_string(),
                status_message_ts: "3000.200".to_string(),
            },
            "✅ Finalizing response...".to_string(),
        )]);
    }
}
