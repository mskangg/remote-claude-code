//! Slack transport layer for Remote Claude Code.
//!
//! Provides [`serve_socket_mode`] (the main WebSocket listener), [`SlackTransport`]
//! (thread→session binding and message routing), and [`SlackWebApiPublisher`]
//! (Slack API calls for posting/updating/deleting messages).  This crate owns
//! Slack payload parsing but does **not** own product business logic.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use core_model::{
    SessionId, SessionMsg, SessionState, TransportBinding, TransportStatusMessage, UserCommand,
};
use core_service::{
    RuntimeEngine, SessionHandle, SessionRegistry, SessionRepository, SessionRuntimeCleanup,
    SessionRuntimeConfigurator,
};
use futures_util::{SinkExt, StreamExt};
use hyper_rustls::HttpsConnectorBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use session_store::SqliteSessionRepository;
use slack_morphism::prelude::*;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackThreadReply {
    pub channel_id: String,
    pub thread_ts: String,
    pub text: String,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackSessionStart {
    pub channel_id: String,
    pub thread_ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackProject {
    pub project_root: String,
    pub project_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackListedSession {
    pub session_id: SessionId,
    pub tmux_session_name: String,
    pub thread_ts: String,
    pub project_label: String,
    pub state: SessionState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedSlackSession {
    pub session_id: SessionId,
    pub state: SessionState,
    pub binding: TransportBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackMessageTarget {
    pub channel_id: String,
    pub thread_ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPostedMessage {
    pub channel_id: String,
    pub message_ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlackFormattedMessage {
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackThreadAction {
    OpenCommandPalette,
    Interrupt,
    SendKey { key: String },
    SendCommand { text: String },
    Terminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackThreadStatus {
    pub channel_id: String,
    pub thread_ts: String,
    pub status_message_ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackEnvelope {
    pub channel: Option<String>,
    pub text: Option<String>,
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    pub subtype: Option<String>,
}

pub fn parse_thread_reply(envelope: SlackEnvelope) -> Option<SlackThreadReply> {
    if envelope.bot_id.is_some() {
        return None;
    }

    if let Some(ref subtype) = envelope.subtype {
        if subtype != "thread_broadcast" {
            return None;
        }
    }

    Some(SlackThreadReply {
        channel_id: envelope.channel?,
        thread_ts: envelope.thread_ts?,
        text: envelope.text?,
        user_id: envelope.user?,
    })
}

pub fn parse_push_thread_reply(event: &SlackPushEventCallback) -> Option<SlackThreadReply> {
    let SlackEventCallbackBody::Message(message) = &event.event else {
        return None;
    };

    let channel_id = message.origin.channel.as_ref()?.to_string();
    let thread_ts = message.origin.thread_ts.as_ref()?.to_string();
    let text = message.content.as_ref()?.text.clone()?;
    let user_id = message.sender.user.as_ref()?.to_string();

    if message.sender.bot_id.is_some() {
        return None;
    }

    if let Some(subtype) = &message.subtype {
        if *subtype != SlackMessageEventType::ThreadBroadcast {
            return None;
        }
    }

    Some(SlackThreadReply {
        channel_id,
        thread_ts,
        text,
        user_id,
    })
}

pub fn build_thread_message_request(
    target: &SlackMessageTarget,
    text: impl Into<String>,
) -> SlackApiChatPostMessageRequest {
    SlackApiChatPostMessageRequest::new(
        SlackChannelId(target.channel_id.clone()),
        SlackMessageContent::new().with_text(text.into()),
    )
    .with_thread_ts(SlackTs(target.thread_ts.clone()))
}

pub fn build_thread_message_request_with_blocks(
    target: &SlackMessageTarget,
    text: impl Into<String>,
    blocks: Vec<SlackBlock>,
) -> SlackApiChatPostMessageRequest {
    SlackApiChatPostMessageRequest::new(
        SlackChannelId(target.channel_id.clone()),
        SlackMessageContent {
            text: Some(text.into()),
            blocks: Some(blocks),
            attachments: None,
            upload: None,
            files: None,
            reactions: None,
            metadata: None,
        },
    )
    .with_thread_ts(SlackTs(target.thread_ts.clone()))
}

pub fn build_channel_message_request(
    channel_id: impl Into<String>,
    text: impl Into<String>,
) -> SlackApiChatPostMessageRequest {
    SlackApiChatPostMessageRequest::new(
        SlackChannelId(channel_id.into()),
        SlackMessageContent::new().with_text(text.into()),
    )
}

const SLACK_FINAL_REPLY_TEXT_LIMIT: usize = 2_500;

fn split_for_slack_final_reply(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    // Track as &str cursor to avoid allocating on every iteration.
    let mut remaining = text.trim();

    while remaining.chars().count() > SLACK_FINAL_REPLY_TEXT_LIMIT {
        // Find the byte offset of the SLACK_FINAL_REPLY_TEXT_LIMIT-th char without
        // collecting into a String.
        let byte_limit = remaining
            .char_indices()
            .nth(SLACK_FINAL_REPLY_TEXT_LIMIT)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let slice = &remaining[..byte_limit];

        let split_at = slice
            .rfind("\n\n")
            .map(|i| i + 2)
            .or_else(|| slice.rfind('\n').map(|i| i + 1))
            .or_else(|| slice.rfind(' ').map(|i| i + 1))
            .unwrap_or(byte_limit);

        let chunk = remaining[..split_at].trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string()); // single allocation per chunk
        }
        remaining = remaining[split_at..].trim();
    }

    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }

    chunks
}

/// Strip inline markdown (`**bold**`, `` `code` ``, fenced blocks) in a single pass.
fn strip_markdown_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next(); // skip second *
            }
            '`' => {}
            _ => out.push(c),
        }
    }
    out
}

fn to_plain_fallback(text: &str) -> String {
    // Longest prefix first so "### " is matched before "# ".
    const HEADING_PREFIXES: &[&str] = &["### ", "## ", "# "];

    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if let Some(rest) = HEADING_PREFIXES.iter().find_map(|p| trimmed.strip_prefix(p)) {
                return rest.trim().to_string();
            }
            let mut s = strip_markdown_inline(line);
            s.truncate(s.trim_end().len());
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn format_claude_text_for_slack(text: &str) -> Vec<SlackFormattedMessage> {
    split_for_slack_final_reply(text)
        .into_iter()
        .map(|chunk| SlackFormattedMessage {
            text: to_plain_fallback(&chunk),
        })
        .collect()
}

pub fn build_status_update_request(
    posted: &SlackPostedMessage,
    text: impl Into<String>,
) -> SlackApiChatUpdateRequest {
    SlackApiChatUpdateRequest::new(
        SlackChannelId(posted.channel_id.clone()),
        SlackMessageContent::new().with_text(text.into()),
        SlackTs(posted.message_ts.clone()),
    )
}

pub fn build_status_delete_request(status: &SlackThreadStatus) -> SlackApiChatDeleteRequest {
    SlackApiChatDeleteRequest {
        channel: SlackChannelId(status.channel_id.clone()),
        ts: SlackTs(status.status_message_ts.clone()),
        as_user: None,
    }
}

#[async_trait]
pub trait SessionBindingStore: Send + Sync {
    async fn find_session_id(&self, binding: &TransportBinding) -> Result<Option<SessionId>>;
    async fn find_binding(&self, session_id: SessionId) -> Result<Option<TransportBinding>>;
}

#[async_trait]
pub trait SessionBindingRegistrar: Send + Sync {
    async fn save_binding(&self, binding: &TransportBinding, session_id: SessionId) -> Result<()>;
}

#[async_trait]
pub trait SessionStatusStore: Send + Sync {
    async fn find_status_message(
        &self,
        binding: &TransportBinding,
    ) -> Result<Option<TransportStatusMessage>>;
}

#[async_trait]
pub trait SessionStatusRegistrar: Send + Sync {
    async fn save_status_message(&self, status: &TransportStatusMessage) -> Result<()>;
}

#[async_trait]
pub trait SessionHandleResolver: Send + Sync {
    async fn resolve(&self, session_id: SessionId) -> Result<SessionHandle>;
}

#[async_trait]
pub trait SlackProjectLocator: Send + Sync {
    async fn find_project(&self, channel_id: &str) -> Result<Option<SlackProject>>;
}

#[async_trait]
pub trait SlackSessionCatalogStore: Send + Sync {
    async fn list_channel_sessions(&self, channel_id: &str) -> Result<Vec<SlackListedSession>>;
}

#[async_trait]
impl<T> SessionBindingStore for Arc<T>
where
    T: SessionBindingStore + Send + Sync,
{
    async fn find_session_id(&self, binding: &TransportBinding) -> Result<Option<SessionId>> {
        (**self).find_session_id(binding).await
    }

    async fn find_binding(&self, session_id: SessionId) -> Result<Option<TransportBinding>> {
        (**self).find_binding(session_id).await
    }
}

#[async_trait]
impl<T> SessionBindingRegistrar for Arc<T>
where
    T: SessionBindingRegistrar + Send + Sync,
{
    async fn save_binding(&self, binding: &TransportBinding, session_id: SessionId) -> Result<()> {
        (**self).save_binding(binding, session_id).await
    }
}

#[async_trait]
impl<T> SessionStatusStore for Arc<T>
where
    T: SessionStatusStore + Send + Sync,
{
    async fn find_status_message(
        &self,
        binding: &TransportBinding,
    ) -> Result<Option<TransportStatusMessage>> {
        (**self).find_status_message(binding).await
    }
}

#[async_trait]
impl<T> SessionStatusRegistrar for Arc<T>
where
    T: SessionStatusRegistrar + Send + Sync,
{
    async fn save_status_message(&self, status: &TransportStatusMessage) -> Result<()> {
        (**self).save_status_message(status).await
    }
}

#[async_trait]
impl<T> SessionHandleResolver for Arc<T>
where
    T: SessionHandleResolver + Send + Sync,
{
    async fn resolve(&self, session_id: SessionId) -> Result<SessionHandle> {
        (**self).resolve(session_id).await
    }
}

#[async_trait]
impl<T> SlackProjectLocator for Arc<T>
where
    T: SlackProjectLocator + Send + Sync,
{
    async fn find_project(&self, channel_id: &str) -> Result<Option<SlackProject>> {
        (**self).find_project(channel_id).await
    }
}

#[async_trait]
impl<T> SlackSessionCatalogStore for Arc<T>
where
    T: SlackSessionCatalogStore + Send + Sync,
{
    async fn list_channel_sessions(&self, channel_id: &str) -> Result<Vec<SlackListedSession>> {
        (**self).list_channel_sessions(channel_id).await
    }
}

#[async_trait]
impl<T> SlackThreadRouter for Arc<T>
where
    T: SlackThreadRouter + Send + Sync,
{
    async fn route_thread_reply(&self, reply: SlackThreadReply) -> Result<SessionState> {
        (**self).route_thread_reply(reply).await
    }
}

#[async_trait]
impl<T> SlackSessionStarter for Arc<T>
where
    T: SlackSessionStarter + Send + Sync,
{
    async fn start_slack_session(&self, start: SlackSessionStart) -> Result<StartedSlackSession> {
        (**self).start_slack_session(start).await
    }
}

#[async_trait]
impl<T> SlackSessionOrchestrator for Arc<T>
where
    T: SlackSessionOrchestrator + Send + Sync,
{
    async fn start_new_session(&self, channel_id: &str) -> Result<StartedSlackSession> {
        (**self).start_new_session(channel_id).await
    }

    async fn handle_session_reply(&self, reply: SlackThreadReply) -> Result<SessionState> {
        (**self).handle_session_reply(reply).await
    }

    async fn list_channel_sessions(&self, channel_id: &str) -> Result<Vec<SlackListedSession>> {
        (**self).list_channel_sessions(channel_id).await
    }

    async fn post_session_list(&self, channel_id: &str, thread_ts: &str) -> Result<()> {
        (**self).post_session_list(channel_id, thread_ts).await
    }

    async fn handle_thread_action(
        &self,
        channel_id: &str,
        thread_ts: &str,
        action: SlackThreadAction,
    ) -> Result<Option<SessionState>> {
        (**self)
            .handle_thread_action(channel_id, thread_ts, action)
            .await
    }
}

#[async_trait]
impl<R, E> SessionHandleResolver for SessionRegistry<R, E>
where
    R: SessionRepository + Send + Sync + 'static,
    E: RuntimeEngine + SessionRuntimeCleanup + Send + Sync + 'static,
{
    async fn resolve(&self, session_id: SessionId) -> Result<SessionHandle> {
        Ok(self.session(session_id).await)
    }
}

#[async_trait]
pub trait SlackThreadRouter: Send + Sync {
    async fn route_thread_reply(&self, reply: SlackThreadReply) -> Result<SessionState>;
}

#[async_trait]
pub trait SlackSessionStarter: Send + Sync {
    async fn start_slack_session(&self, start: SlackSessionStart) -> Result<StartedSlackSession>;
}

#[async_trait]
pub trait SlackSessionOrchestrator: Send + Sync {
    async fn start_new_session(&self, channel_id: &str) -> Result<StartedSlackSession>;
    async fn handle_session_reply(&self, reply: SlackThreadReply) -> Result<SessionState>;
    async fn list_channel_sessions(&self, channel_id: &str) -> Result<Vec<SlackListedSession>>;
    async fn post_session_list(&self, channel_id: &str, thread_ts: &str) -> Result<()>;
    async fn handle_thread_action(
        &self,
        channel_id: &str,
        thread_ts: &str,
        action: SlackThreadAction,
    ) -> Result<Option<SessionState>>;
}

#[async_trait]
pub trait SlackSessionPublisher: Send + Sync {
    async fn post_channel_message(&self, channel_id: &str, text: &str) -> Result<SlackPostedMessage>;
    async fn post_thread_message_with_blocks(
        &self,
        target: &SlackMessageTarget,
        text: &str,
        blocks: Vec<SlackBlock>,
    ) -> Result<SlackPostedMessage>;
    async fn update_working_status(&self, status: &SlackThreadStatus, text: &str) -> Result<()>;
    async fn delete_message(&self, status: &SlackThreadStatus) -> Result<()>;
    async fn get_message_permalink(&self, channel_id: &str, message_ts: &str) -> Result<String>;
    async fn post_final_reply(
        &self,
        target: &SlackMessageTarget,
        text: &str,
    ) -> Result<SlackPostedMessage>;
}

pub trait SlackStatusMessagePublisher: SlackSessionPublisher + SlackWorkingStatusPublisher {}

impl<T> SlackStatusMessagePublisher for T where T: SlackSessionPublisher + SlackWorkingStatusPublisher {}

pub struct InMemorySlackBindingStore {
    bindings: RwLock<HashMap<TransportBinding, SessionId>>,
    statuses: RwLock<HashMap<TransportBinding, TransportStatusMessage>>,
}

impl InMemorySlackBindingStore {
    pub fn new() -> Self {
        Self {
            bindings: RwLock::new(HashMap::new()),
            statuses: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemorySlackBindingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySlackBindingStore {

    pub async fn insert(&self, binding: TransportBinding, session_id: SessionId) {
        self.bindings.write().await.insert(binding, session_id);
    }
}

#[async_trait]
impl SessionBindingStore for InMemorySlackBindingStore {
    async fn find_session_id(&self, binding: &TransportBinding) -> Result<Option<SessionId>> {
        Ok(self.bindings.read().await.get(binding).copied())
    }

    async fn find_binding(&self, session_id: SessionId) -> Result<Option<TransportBinding>> {
        Ok(self
            .bindings
            .read()
            .await
            .iter()
            .find_map(|(binding, candidate)| (*candidate == session_id).then(|| binding.clone())))
    }
}

#[async_trait]
impl SessionBindingRegistrar for InMemorySlackBindingStore {
    async fn save_binding(&self, binding: &TransportBinding, session_id: SessionId) -> Result<()> {
        self.insert(binding.clone(), session_id).await;
        Ok(())
    }
}

#[async_trait]
impl SessionStatusStore for InMemorySlackBindingStore {
    async fn find_status_message(
        &self,
        binding: &TransportBinding,
    ) -> Result<Option<TransportStatusMessage>> {
        Ok(self.statuses.read().await.get(binding).cloned())
    }
}

#[async_trait]
impl SessionStatusRegistrar for InMemorySlackBindingStore {
    async fn save_status_message(&self, status: &TransportStatusMessage) -> Result<()> {
        self.statuses
            .write()
            .await
            .insert(status.binding.clone(), status.clone());
        Ok(())
    }
}

#[async_trait]
impl SlackSessionCatalogStore for InMemorySlackBindingStore {
    async fn list_channel_sessions(&self, _channel_id: &str) -> Result<Vec<SlackListedSession>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl SessionBindingStore for SqliteSessionRepository {
    async fn find_session_id(&self, binding: &TransportBinding) -> Result<Option<SessionId>> {
        self.find_transport_binding_session_id(binding)
    }

    async fn find_binding(&self, session_id: SessionId) -> Result<Option<TransportBinding>> {
        self.find_transport_binding(session_id)
    }
}

#[async_trait]
impl SessionBindingRegistrar for SqliteSessionRepository {
    async fn save_binding(&self, binding: &TransportBinding, session_id: SessionId) -> Result<()> {
        self.save_transport_binding(binding, session_id)
    }
}

#[async_trait]
impl SessionStatusStore for SqliteSessionRepository {
    async fn find_status_message(
        &self,
        binding: &TransportBinding,
    ) -> Result<Option<TransportStatusMessage>> {
        self.find_transport_status_message(binding)
    }
}

#[async_trait]
impl SlackSessionCatalogStore for SqliteSessionRepository {
    async fn list_channel_sessions(&self, channel_id: &str) -> Result<Vec<SlackListedSession>> {
        let stored = SqliteSessionRepository::list_channel_sessions(self, channel_id)?;
        Ok(stored
            .into_iter()
            .map(|session| SlackListedSession {
                session_id: session.session_id,
                tmux_session_name: session.session_id.0.to_string(),
                thread_ts: session.thread_ts,
                project_label: String::new(),
                state: session.state,
            })
            .collect())
    }
}

#[async_trait]
impl SessionStatusRegistrar for SqliteSessionRepository {
    async fn save_status_message(&self, status: &TransportStatusMessage) -> Result<()> {
        self.save_transport_status_message(status)
    }
}

pub struct SlackTransport<S, R, C> {
    store: Arc<S>,
    resolver: Arc<R>,
    configurator: Arc<C>,
}

impl<S, R, C> SlackTransport<S, R, C>
where
    S: SessionBindingStore,
    R: SessionHandleResolver,
    C: SessionRuntimeConfigurator,
{
    pub fn new(store: Arc<S>, resolver: Arc<R>, configurator: Arc<C>) -> Self {
        Self {
            store,
            resolver,
            configurator,
        }
    }

    pub fn configurator(&self) -> &Arc<C> {
        &self.configurator
    }

    pub async fn handle_thread_reply(&self, reply: SlackThreadReply) -> Result<SessionState> {
        let binding = TransportBinding {
            project_space_id: reply.channel_id,
            session_space_id: reply.thread_ts,
        };
        self.send_session_message(
            &binding,
            SessionMsg::UserCommand(UserCommand { text: reply.text }),
        )
        .await
    }

    pub async fn handle_thread_action(
        &self,
        channel_id: &str,
        thread_ts: &str,
        action: SlackThreadAction,
    ) -> Result<SessionState> {
        let binding = TransportBinding {
            project_space_id: channel_id.to_string(),
            session_space_id: thread_ts.to_string(),
        };
        let message = match action {
            SlackThreadAction::OpenCommandPalette => {
                return self
                    .store
                    .find_session_id(&binding)
                    .await?
                    .map(|_| SessionState::Idle)
                    .ok_or_else(|| anyhow::anyhow!("no session binding for Slack thread"));
            }
            SlackThreadAction::Interrupt => SessionMsg::Interrupt,
            SlackThreadAction::SendKey { key } => SessionMsg::SendKey { key },
            SlackThreadAction::SendCommand { text } => SessionMsg::UserCommand(UserCommand { text }),
            SlackThreadAction::Terminate => SessionMsg::Terminate,
        };
        self.send_session_message(&binding, message).await
    }

    async fn send_session_message(
        &self,
        binding: &TransportBinding,
        message: SessionMsg,
    ) -> Result<SessionState> {
        let session_id = self
            .store
            .find_session_id(binding)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no session binding for Slack thread"))?;
        let handle = self.resolver.resolve(session_id).await?;
        handle.send(message).await
    }

    pub async fn bind_thread(
        &self,
        channel_id: impl Into<String>,
        thread_ts: impl Into<String>,
        session_id: SessionId,
    ) -> Result<()>
    where
        S: SessionBindingRegistrar,
    {
        self.store
            .save_binding(
                &TransportBinding {
                    project_space_id: channel_id.into(),
                    session_space_id: thread_ts.into(),
                },
                session_id,
            )
            .await
    }

    pub async fn start_session(
        &self,
        start: SlackSessionStart,
        project_root: &str,
    ) -> Result<StartedSlackSession>
    where
        S: SessionBindingRegistrar,
    {
        let session_id = SessionId::new();
        let binding = TransportBinding {
            project_space_id: start.channel_id,
            session_space_id: start.thread_ts,
        };

        self.store.save_binding(&binding, session_id).await?;
        self.configurator
            .register_project_root(session_id, project_root)
            .await?;
        let handle = self.resolver.resolve(session_id).await?;
        let state = handle.send(SessionMsg::Recover).await?;

        Ok(StartedSlackSession {
            session_id,
            state,
            binding,
        })
    }

    pub async fn start_session_with_working_status<P>(
        &self,
        start: SlackSessionStart,
        project_root: &str,
        publisher: &P,
    ) -> Result<StartedSlackSession>
    where
        S: SessionBindingRegistrar + SessionStatusRegistrar,
        P: SlackWorkingStatusPublisher,
    {
        let started = self.start_session(start, project_root).await?;
        let target = SlackMessageTarget {
            channel_id: started.binding.project_space_id.clone(),
            thread_ts: started.binding.session_space_id.clone(),
        };
        let status = publisher.post_working_status(&target, "⏳ Working...").await?;

        self.store
            .save_status_message(&TransportStatusMessage {
                binding: started.binding.clone(),
                status_message_id: status.status_message_ts,
            })
            .await?;

        Ok(started)
    }

    pub async fn update_working_status<P>(
        &self,
        binding: &TransportBinding,
        publisher: &P,
        text: &str,
    ) -> Result<()>
    where
        S: SessionStatusStore,
        P: SlackSessionPublisher,
    {
        let status = self
            .store
            .find_status_message(binding)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no working status recorded for Slack thread"))?;

        publisher
            .update_working_status(
                &SlackThreadStatus {
                    channel_id: status.binding.project_space_id,
                    thread_ts: status.binding.session_space_id,
                    status_message_ts: status.status_message_id,
                },
                text,
            )
            .await
    }

    pub async fn ensure_working_status<P>(
        &self,
        binding: &TransportBinding,
        publisher: &P,
        text: &str,
    ) -> Result<()>
    where
        S: SessionStatusStore + SessionStatusRegistrar,
        P: SlackStatusMessagePublisher,
    {
        if let Some(status) = self.store.find_status_message(binding).await? {
            let result = publisher
                .update_working_status(
                    &SlackThreadStatus {
                        channel_id: status.binding.project_space_id.clone(),
                        thread_ts: status.binding.session_space_id.clone(),
                        status_message_ts: status.status_message_id.clone(),
                    },
                    text,
                )
                .await;
            if result.is_ok() {
                return Ok(());
            }
        }

        let posted = publisher
            .post_working_status(
                &SlackMessageTarget {
                    channel_id: binding.project_space_id.clone(),
                    thread_ts: binding.session_space_id.clone(),
                },
                text.to_string(),
            )
            .await?;
        self.store
            .save_status_message(&TransportStatusMessage {
                binding: binding.clone(),
                status_message_id: posted.status_message_ts,
            })
            .await?;
        Ok(())
    }

    pub async fn post_final_reply<P>(
        &self,
        binding: &TransportBinding,
        publisher: &P,
        text: &str,
    ) -> Result<SlackPostedMessage>
    where
        P: SlackSessionPublisher,
    {
        publisher
            .post_final_reply(
                &SlackMessageTarget {
                    channel_id: binding.project_space_id.clone(),
                    thread_ts: binding.session_space_id.clone(),
                },
                text,
            )
            .await
    }

    pub async fn list_channel_sessions(&self, channel_id: &str) -> Result<Vec<SlackListedSession>>
    where
        S: SlackSessionCatalogStore,
    {
        self.store.list_channel_sessions(channel_id).await
    }
}

#[async_trait]
impl<S, R, C> SlackThreadRouter for SlackTransport<S, R, C>
where
    S: SessionBindingStore + Send + Sync,
    R: SessionHandleResolver + Send + Sync,
    C: SessionRuntimeConfigurator + Send + Sync,
{
    async fn route_thread_reply(&self, reply: SlackThreadReply) -> Result<SessionState> {
        self.handle_thread_reply(reply).await
    }
}

#[async_trait]
impl<S, R, C> SlackSessionStarter for SlackTransport<S, R, C>
where
    S: SessionBindingStore + SessionBindingRegistrar + Send + Sync,
    R: SessionHandleResolver + Send + Sync,
    C: SessionRuntimeConfigurator + Send + Sync,
{
    async fn start_slack_session(&self, start: SlackSessionStart) -> Result<StartedSlackSession> {
        self.start_session(start, ".").await
    }
}

pub fn parse_allowed_user_ids(env_value: &str) -> Vec<String> {
    env_value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub fn is_allowed_user(user_id: &str, allowed: &[String]) -> bool {
    // Fail-closed: an empty allowlist denies everyone. Callers must ensure a
    // non-empty allowlist is configured (enforced at startup by from_env()).
    !allowed.is_empty() && allowed.iter().any(|id| id == user_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackSocketModeConfig {
    pub bot_token: String,
    pub app_token: String,
    pub allowed_user_ids: Vec<String>,
}

impl SlackSocketModeConfig {
    pub fn from_env() -> Result<Self> {
        let allowed_user_ids = std::env::var("SLACK_ALLOWED_USER_ID")
            .map(|v| parse_allowed_user_ids(&v))
            .unwrap_or_default();
        if allowed_user_ids.is_empty() {
            anyhow::bail!("SLACK_ALLOWED_USER_ID is not set or empty — set at least one allowed Slack user ID");
        }
        Ok(Self {
            bot_token: std::env::var("SLACK_BOT_TOKEN")?,
            app_token: std::env::var("SLACK_APP_TOKEN")?,
            allowed_user_ids,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlackSlashCommandPayload {
    pub command: String,
    pub channel_id: String,
    pub user_id: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlackBlockActionPayload {
    channel_id: String,
    thread_ts: Option<String>,
    action_id: String,
    value: Option<String>,
    user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawInteractiveUser {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawInteractivePayload {
    #[serde(rename = "type")]
    kind: String,
    user: Option<RawInteractiveUser>,
    channel: Option<RawInteractiveChannel>,
    message: Option<RawInteractiveMessage>,
    container: Option<RawInteractiveContainer>,
    actions: Option<Vec<RawInteractiveAction>>,
}

#[derive(Debug, Deserialize)]
struct RawInteractiveChannel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawInteractiveMessage {
    thread_ts: Option<String>,
    ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawInteractiveContainer {
    channel_id: Option<String>,
    message_ts: Option<String>,
    // Present in Slack payloads when the action originates from a threaded message.
    thread_ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawInteractiveAction {
    action_id: String,
    value: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum SocketModeRequest {
    Hello { app_id: String, num_connections: u32 },
    Disconnect { reason: String },
    SlashCommand {
        envelope_id: String,
        payload: SlackSlashCommandPayload,
    },
    EventsApi {
        envelope_id: String,
        payload: Box<SlackPushEventCallback>,
    },
    Interactive {
        envelope_id: String,
        action: Option<SlackBlockActionPayload>,
    },
    Unknown {
        envelope_id: Option<String>,
        kind: String,
    },
}

#[derive(Debug, Deserialize)]
struct RawSocketModeEnvelope {
    #[serde(rename = "type")]
    kind: String,
    envelope_id: Option<String>,
    payload: Option<Value>,
    connection_info: Option<RawSocketModeConnectionInfo>,
    num_connections: Option<u32>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSocketModeConnectionInfo {
    app_id: String,
}

fn parse_socket_mode_request(raw: &str) -> Result<SocketModeRequest> {
    let envelope: RawSocketModeEnvelope = serde_json::from_str(raw)?;

    match envelope.kind.as_str() {
        "hello" => Ok(SocketModeRequest::Hello {
            app_id: envelope
                .connection_info
                .ok_or_else(|| anyhow::anyhow!("missing connection_info for hello event"))?
                .app_id,
            num_connections: envelope.num_connections.unwrap_or(0),
        }),
        "disconnect" => Ok(SocketModeRequest::Disconnect {
            reason: envelope.reason.unwrap_or_else(|| "unknown".to_string()),
        }),
        "slash_commands" => Ok(SocketModeRequest::SlashCommand {
            envelope_id: envelope
                .envelope_id
                .ok_or_else(|| anyhow::anyhow!("missing envelope_id for slash command"))?,
            payload: serde_json::from_value(
                envelope
                    .payload
                    .ok_or_else(|| anyhow::anyhow!("missing slash command payload"))?,
            )?,
        }),
        "events_api" => Ok(SocketModeRequest::EventsApi {
            envelope_id: envelope
                .envelope_id
                .ok_or_else(|| anyhow::anyhow!("missing envelope_id for events_api"))?,
            payload: Box::new(serde_json::from_value(
                envelope
                    .payload
                    .ok_or_else(|| anyhow::anyhow!("missing events_api payload"))?,
            )?),
        }),
        "interactive" => Ok(SocketModeRequest::Interactive {
            envelope_id: envelope
                .envelope_id
                .ok_or_else(|| anyhow::anyhow!("missing envelope_id for interactive event"))?,
            action: parse_interactive_action(envelope.payload)?,
        }),
        other => Ok(SocketModeRequest::Unknown {
            envelope_id: envelope.envelope_id,
            kind: other.to_string(),
        }),
    }
}

fn build_socket_mode_ack(envelope_id: &str, payload: Option<Value>) -> Result<String> {
    let mut body = json!({
        "envelope_id": envelope_id,
    });

    if let Some(payload) = payload {
        body["payload"] = payload;
    }

    Ok(serde_json::to_string(&body)?)
}

fn parse_interactive_action(payload: Option<Value>) -> Result<Option<SlackBlockActionPayload>> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    let payload: RawInteractivePayload = serde_json::from_value(payload)?;
    if payload.kind != "block_actions" {
        return Ok(None);
    }

    let channel_id = payload
        .channel
        .as_ref()
        .map(|channel| channel.id.clone())
        .or_else(|| payload.container.as_ref().and_then(|container| container.channel_id.clone()));
    let Some(channel_id) = channel_id else {
        return Ok(None);
    };
    let Some(action) = payload.actions.and_then(|mut actions| actions.drain(..).next()) else {
        return Ok(None);
    };

    // Priority: message.thread_ts > container.thread_ts > message.ts > container.message_ts.
    // container.thread_ts is present in Slack payloads when the action originates from a
    // threaded message, providing a reliable fallback when message.thread_ts is absent.
    let thread_ts = {
        let from_message_thread = payload.message.as_ref().and_then(|m| m.thread_ts.clone());
        let from_container_thread = payload.container.as_ref().and_then(|c| c.thread_ts.clone());
        let from_message_ts = payload.message.as_ref().and_then(|m| m.ts.clone());
        let from_container_message = payload.container.and_then(|c| c.message_ts);
        from_message_thread
            .or(from_container_thread)
            .or(from_message_ts)
            .or(from_container_message)
    };

    if thread_ts.is_none() {
        tracing::warn!(action_id = action.action_id, channel_id, "interactive action has no resolvable thread_ts; cannot route to session");
    }

    Ok(Some(SlackBlockActionPayload {
        channel_id,
        thread_ts,
        action_id: action.action_id,
        value: action.value,
        user_id: payload.user.map(|u| u.id),
    }))
}

fn build_main_menu_response() -> Value {
    json!({
        "text": "Choose an action",
        "blocks": [
            {
                "type": "actions",
                "elements": [
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": "Start new session" },
                        "action_id": "claude_session_new",
                        "value": "claude.session.new"
                    },
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": "View existing sessions" },
                        "action_id": "claude_session_list",
                        "value": "claude.session.list"
                    }
                ]
            }
        ]
    })
}

pub struct SlackWebApiPublisher {
    client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    bot_token: SlackApiToken,
}

fn build_slack_https_connector() -> SlackClientHyperHttpsConnector {
    HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http2()
        .build()
        .into()
}

#[async_trait]
pub trait SlackWorkingStatusPublisher: Send + Sync {
    async fn post_working_status(
        &self,
        target: &SlackMessageTarget,
        text: impl Into<String> + Send,
    ) -> Result<SlackThreadStatus>;
}

impl SlackWebApiPublisher {
    pub fn new(bot_token: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: Arc::new(SlackClient::new(build_slack_https_connector())),
            bot_token: SlackApiToken::new(bot_token.into().into()),
        })
    }

    pub async fn post_thread_message(
        &self,
        target: &SlackMessageTarget,
        text: impl Into<String>,
    ) -> Result<SlackPostedMessage> {
        let session = self.client.open_session(&self.bot_token);
        let response = session
            .chat_post_message(&build_thread_message_request(target, text))
            .await?;

        Ok(SlackPostedMessage {
            channel_id: response.channel.to_string(),
            message_ts: response.ts.to_string(),
        })
    }

    pub async fn post_thread_message_with_blocks(
        &self,
        target: &SlackMessageTarget,
        text: impl Into<String>,
        blocks: Vec<SlackBlock>,
    ) -> Result<SlackPostedMessage> {
        let session = self.client.open_session(&self.bot_token);
        let response = session
            .chat_post_message(&build_thread_message_request_with_blocks(target, text, blocks))
            .await?;

        Ok(SlackPostedMessage {
            channel_id: response.channel.to_string(),
            message_ts: response.ts.to_string(),
        })
    }

    pub async fn post_channel_message(
        &self,
        channel_id: &str,
        text: impl Into<String>,
    ) -> Result<SlackPostedMessage> {
        let session = self.client.open_session(&self.bot_token);
        let response = session
            .chat_post_message(&build_channel_message_request(channel_id, text))
            .await?;

        Ok(SlackPostedMessage {
            channel_id: response.channel.to_string(),
            message_ts: response.ts.to_string(),
        })
    }

    pub async fn get_message_permalink(
        &self,
        channel_id: &str,
        message_ts: &str,
    ) -> Result<Url> {
        let session = self.client.open_session(&self.bot_token);
        let response = session
            .chat_get_permalink(&SlackApiChatGetPermalinkRequest {
                channel: SlackChannelId(channel_id.to_string()),
                message_ts: SlackTs(message_ts.to_string()),
            })
            .await?;
        Ok(response.permalink)
    }

    pub async fn update_message(
        &self,
        posted: &SlackPostedMessage,
        text: impl Into<String>,
    ) -> Result<()> {
        let session = self.client.open_session(&self.bot_token);
        session
            .chat_update(&build_status_update_request(posted, text))
            .await?;
        Ok(())
    }

    pub async fn post_working_status(
        &self,
        target: &SlackMessageTarget,
        text: impl Into<String>,
    ) -> Result<SlackThreadStatus> {
        let posted = self.post_thread_message(target, text).await?;

        Ok(SlackThreadStatus {
            channel_id: posted.channel_id,
            thread_ts: target.thread_ts.clone(),
            status_message_ts: posted.message_ts,
        })
    }

    pub async fn update_working_status(
        &self,
        status: &SlackThreadStatus,
        text: impl Into<String>,
    ) -> Result<()> {
        self.update_message(
            &SlackPostedMessage {
                channel_id: status.channel_id.clone(),
                message_ts: status.status_message_ts.clone(),
            },
            text,
        )
        .await
    }

    pub async fn delete_message(&self, status: &SlackThreadStatus) -> Result<()> {
        let session = self.client.open_session(&self.bot_token);
        session
            .chat_delete(&build_status_delete_request(status))
            .await?;
        Ok(())
    }

    pub async fn post_final_reply(
        &self,
        target: &SlackMessageTarget,
        text: impl Into<String>,
    ) -> Result<SlackPostedMessage> {
        let text = text.into();
        let mut last_posted = None;

        for message in format_claude_text_for_slack(&text) {
            let posted = self.post_thread_message(target, &message.text).await?;
            last_posted = Some(posted);
        }

        last_posted.ok_or_else(|| anyhow::anyhow!("final reply text is empty"))
    }
}

#[async_trait]
impl SlackWorkingStatusPublisher for SlackWebApiPublisher {
    async fn post_working_status(
        &self,
        target: &SlackMessageTarget,
        text: impl Into<String> + Send,
    ) -> Result<SlackThreadStatus> {
        SlackWebApiPublisher::post_working_status(self, target, text).await
    }
}

#[async_trait]
impl SlackSessionPublisher for SlackWebApiPublisher {
    async fn post_channel_message(&self, channel_id: &str, text: &str) -> Result<SlackPostedMessage> {
        SlackWebApiPublisher::post_channel_message(self, channel_id, text).await
    }

    async fn post_thread_message_with_blocks(
        &self,
        target: &SlackMessageTarget,
        text: &str,
        blocks: Vec<SlackBlock>,
    ) -> Result<SlackPostedMessage> {
        SlackWebApiPublisher::post_thread_message_with_blocks(self, target, text, blocks).await
    }

    async fn update_working_status(&self, status: &SlackThreadStatus, text: &str) -> Result<()> {
        SlackWebApiPublisher::update_working_status(self, status, text).await
    }

    async fn delete_message(&self, status: &SlackThreadStatus) -> Result<()> {
        SlackWebApiPublisher::delete_message(self, status).await
    }

    async fn get_message_permalink(&self, channel_id: &str, message_ts: &str) -> Result<String> {
        Ok(SlackWebApiPublisher::get_message_permalink(self, channel_id, message_ts)
            .await?
            .to_string())
    }

    async fn post_final_reply(
        &self,
        target: &SlackMessageTarget,
        text: &str,
    ) -> Result<SlackPostedMessage> {
        SlackWebApiPublisher::post_final_reply(self, target, text).await
    }
}


pub async fn serve_socket_mode(
    orchestrator: Arc<dyn SlackSessionOrchestrator>,
    config: SlackSocketModeConfig,
) -> Result<()> {
    let client = Arc::new(SlackClient::new(build_slack_https_connector()));
    let app_token: SlackApiToken = SlackApiToken::new(config.app_token.clone().into());
    let session = client.open_session(&app_token);

    tracing::info!("socket mode token registered");

    // Reconnect delay grows exponentially on repeated failures.
    let mut reconnect_delay = Duration::from_secs(1);
    const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
    // After this many consecutive open-connection failures the process gives up,
    // preventing an invisible infinite retry loop when the app has a permanent
    // misconfiguration that is not covered by the auth-string allowlist.
    const MAX_CONSECUTIVE_OPEN_FAILURES: u32 = 10;
    let mut consecutive_open_failures: u32 = 0;

    loop {
        // Request a fresh WebSocket URL from Slack. Known auth errors are fatal
        // immediately; other failures are retried up to MAX_CONSECUTIVE_OPEN_FAILURES
        // times before being treated as fatal.
        let open = match session
            .apps_connections_open(&SlackApiAppsConnectionOpenRequest::new())
            .await
        {
            Ok(open) => {
                consecutive_open_failures = 0;
                open
            }
            Err(error) => {
                let msg = error.to_string();
                if msg.contains("invalid_auth")
                    || msg.contains("not_authed")
                    || msg.contains("token_revoked")
                {
                    return Err(anyhow::anyhow!("Slack auth error (not retrying): {error}"));
                }
                consecutive_open_failures += 1;
                if consecutive_open_failures >= MAX_CONSECUTIVE_OPEN_FAILURES {
                    return Err(anyhow::anyhow!(
                        "Slack connection failed after {MAX_CONSECUTIVE_OPEN_FAILURES} consecutive attempts: {error}"
                    ));
                }
                tracing::warn!(
                    error = %error,
                    consecutive_open_failures,
                    reconnect_delay_secs = reconnect_delay.as_secs(),
                    "failed to open socket mode connection; retrying"
                );
                sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                continue;
            }
        };

        let socket_url = open.url.0.to_string();
        tracing::debug!("opening socket mode websocket");

        let (mut stream, _response) = match connect_async(socket_url.as_str()).await {
            Ok(conn) => conn,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    reconnect_delay_secs = reconnect_delay.as_secs(),
                    "failed to connect websocket; retrying"
                );
                sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                continue;
            }
        };

        // Successful connection — reset backoff.
        reconnect_delay = Duration::from_secs(1);
        tracing::info!("socket mode listener started");

        while let Some(message) = stream.next().await {
            match message {
                Ok(Message::Text(body)) => {
                    match handle_socket_mode_text(Arc::clone(&orchestrator), &config.allowed_user_ids, body.as_ref()).await {
                        Ok(Some(reply)) => {
                            if let Err(error) = stream.send(Message::Text(reply.into())).await {
                                tracing::warn!(error = %error, "failed to send ack; reconnecting");
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(error = %error, "non-fatal socket mode handler error");
                        }
                    }
                }
                Ok(Message::Ping(body)) => {
                    if let Err(error) = stream.send(Message::Pong(body)).await {
                        tracing::warn!(error = %error, "failed to send pong; reconnecting");
                        break;
                    }
                }
                Ok(Message::Close(frame)) => {
                    tracing::info!(frame = ?frame, "socket mode websocket closed");
                    break;
                }
                Ok(Message::Binary(_)) => {}
                Ok(Message::Pong(_)) => {}
                Ok(Message::Frame(_)) => {}
                Err(error) => {
                    tracing::error!(error = ?error, "socket mode websocket error");
                    break;
                }
            }
        }

        tracing::info!("reconnecting socket mode websocket");
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
        sleep(reconnect_delay).await;
    }
}

async fn handle_socket_mode_text(
    orchestrator: Arc<dyn SlackSessionOrchestrator>,
    allowed_user_ids: &[String],
    raw: &str,
) -> Result<Option<String>> {
    match parse_socket_mode_request(raw) {
        Ok(SocketModeRequest::Hello {
            app_id,
            num_connections,
        }) => {
            tracing::debug!(app_id, num_connections, "received socket mode hello");
            Ok(None)
        }
        Ok(SocketModeRequest::Disconnect { reason }) => {
            tracing::info!(reason, "received socket mode disconnect");
            Ok(None)
        }
        Ok(SocketModeRequest::SlashCommand {
            envelope_id,
            payload,
        }) => {
            tracing::info!(
                command = payload.command,
                channel_id = payload.channel_id,
                user_id = payload.user_id,
                text = ?payload.text,
                "received slash command"
            );

            if !is_allowed_user(&payload.user_id, allowed_user_ids) {
                tracing::warn!(user_id = payload.user_id, "slash command from non-allowed user");
                return Ok(Some(build_socket_mode_ack(
                    &envelope_id,
                    Some(json!({ "text": "You are not authorized to use this command." })),
                )?));
            }

            let command_text = payload.text.as_deref().map(str::trim).unwrap_or("");
            let ack_payload = if command_text.is_empty() {
                build_main_menu_response()
            } else if !command_requests_new_session(payload.text.as_deref()) {
                json!({
                    "text": "Unsupported command. Use `/cc` or `/cc start`."
                })
            } else {
                let channel_id = payload.channel_id.clone();
                let orchestrator = Arc::clone(&orchestrator);
                tokio::spawn(async move {
                    if let Err(error) = orchestrator.start_new_session(&channel_id).await {
                        tracing::error!(channel_id, error = %error, "failed to start Slack session");
                    }
                });

                json!({
                    "text": "Starting a new Remote Claude Code session. Watch this channel for the new thread."
                })
            };

            Ok(Some(build_socket_mode_ack(&envelope_id, Some(ack_payload))?))
        }
        Ok(SocketModeRequest::EventsApi {
            envelope_id,
            payload,
        }) => {
            if let Some(reply) = parse_push_thread_reply(&payload) {
                if !is_allowed_user(&reply.user_id, allowed_user_ids) {
                    tracing::warn!(user_id = reply.user_id, "thread reply from non-allowed user ignored");
                } else if let Err(error) = orchestrator.handle_session_reply(reply).await {
                    tracing::warn!(error = %error, "failed to handle Slack thread reply");
                }
            }

            Ok(Some(build_socket_mode_ack(&envelope_id, None)?))
        }
        Ok(SocketModeRequest::Interactive {
            envelope_id,
            action,
        }) => {
            if let Some(action) = action {
                tracing::info!(
                    action_id = action.action_id,
                    channel_id = action.channel_id,
                    thread_ts = ?action.thread_ts,
                    value = ?action.value,
                    user_id = ?action.user_id,
                    "received interactive action"
                );

                let Some(action_user_id) = action.user_id.as_deref() else {
                    tracing::warn!(action_id = action.action_id, "interactive action has no user_id; ignoring");
                    return Ok(Some(build_socket_mode_ack(&envelope_id, None)?));
                };
                if !is_allowed_user(action_user_id, allowed_user_ids) {
                    tracing::warn!(user_id = action_user_id, "interactive action from non-allowed user");
                    return Ok(Some(build_socket_mode_ack(&envelope_id, None)?));
                }

                match action.action_id.as_str() {
                    "claude_session_new" => {
                        let channel_id = action.channel_id;
                        let orchestrator = Arc::clone(&orchestrator);
                        tokio::spawn(async move {
                            if let Err(error) = orchestrator.start_new_session(&channel_id).await {
                                tracing::error!(
                                    channel_id,
                                    error = %error,
                                    "failed to start Slack session from interactive action"
                                );
                            }
                        });
                    }
                    "claude_session_list" => {
                        if let Some(thread_ts) = action.thread_ts {
                            if let Err(error) = orchestrator
                                .post_session_list(&action.channel_id, &thread_ts)
                                .await
                            {
                                tracing::warn!(
                                    channel_id = action.channel_id,
                                    thread_ts,
                                    error = %error,
                                    "failed to post session list"
                                );
                            }
                        } else {
                            tracing::warn!(
                                channel_id = action.channel_id,
                                "interactive session list missing thread context"
                            );
                        }
                    }
                    "claude_command_palette_open" => {
                        if let Some(thread_ts) = action.thread_ts {
                            if let Err(error) = orchestrator
                                .handle_thread_action(
                                    &action.channel_id,
                                    &thread_ts,
                                    SlackThreadAction::OpenCommandPalette,
                                )
                                .await
                            {
                                tracing::warn!(
                                    channel_id = action.channel_id,
                                    thread_ts,
                                    error = %error,
                                    "failed to open command palette"
                                );
                            }
                        }
                    }
                    "claude_command_key_interrupt" => {
                        if let Some(thread_ts) = action.thread_ts {
                            if let Err(error) = orchestrator
                                .handle_thread_action(
                                    &action.channel_id,
                                    &thread_ts,
                                    SlackThreadAction::Interrupt,
                                )
                                .await
                            {
                                tracing::warn!(
                                    channel_id = action.channel_id,
                                    thread_ts,
                                    error = %error,
                                    "failed to interrupt session"
                                );
                            }
                        }
                    }
                    "claude_terminal_key_escape" => {
                        if let Some(thread_ts) = action.thread_ts {
                            if let Err(error) = orchestrator
                                .handle_thread_action(
                                    &action.channel_id,
                                    &thread_ts,
                                    SlackThreadAction::SendKey {
                                        key: action
                                            .value
                                            .clone()
                                            .unwrap_or_else(|| "Escape".to_string()),
                                    },
                                )
                                .await
                            {
                                tracing::warn!(
                                    channel_id = action.channel_id,
                                    thread_ts,
                                    error = %error,
                                    "failed to send key to session"
                                );
                            }
                        }
                    }
                    "claude_command_send_clear" => {
                        if let Some(thread_ts) = action.thread_ts {
                            if let Err(error) = orchestrator
                                .handle_thread_action(
                                    &action.channel_id,
                                    &thread_ts,
                                    SlackThreadAction::SendCommand {
                                        text: action
                                            .value
                                            .clone()
                                            .unwrap_or_else(|| "/clear".to_string()),
                                    },
                                )
                                .await
                            {
                                tracing::warn!(
                                    channel_id = action.channel_id,
                                    thread_ts,
                                    error = %error,
                                    "failed to send clear command"
                                );
                            }
                        }
                    }
                    "claude_command_send_revise_claude_md" => {
                        if let Some(thread_ts) = action.thread_ts {
                            if let Err(error) = orchestrator
                                .handle_thread_action(
                                    &action.channel_id,
                                    &thread_ts,
                                    SlackThreadAction::SendCommand {
                                        text: action.value.clone().unwrap_or_else(|| {
                                            "/claude-md-management:revise-claude-md".to_string()
                                        }),
                                    },
                                )
                                .await
                            {
                                tracing::warn!(
                                    channel_id = action.channel_id,
                                    thread_ts,
                                    error = %error,
                                    "failed to send CLAUDE.md update command"
                                );
                            }
                        }
                    }
                    "claude_session_terminate" => {
                        if let Some(thread_ts) = action.thread_ts {
                            if let Err(error) = orchestrator
                                .handle_thread_action(
                                    &action.channel_id,
                                    &thread_ts,
                                    SlackThreadAction::Terminate,
                                )
                                .await
                            {
                                tracing::warn!(
                                    channel_id = action.channel_id,
                                    thread_ts,
                                    error = %error,
                                    "failed to terminate session"
                                );
                            }
                        }
                    }
                    "claude_session_open_thread" => {}
                    other => {
                        tracing::debug!(action_id = other, "ignored interactive action");
                    }
                }
            }
            Ok(Some(build_socket_mode_ack(&envelope_id, None)?))
        }
        Ok(SocketModeRequest::Unknown { envelope_id, kind }) => {
            tracing::debug!(kind, "ignored socket mode event type");
            match envelope_id {
                Some(envelope_id) => Ok(Some(build_socket_mode_ack(&envelope_id, None)?)),
                None => Ok(None),
            }
        }
        Err(error) => {
            tracing::error!(error = %error, raw, "failed to parse socket mode payload");
            Ok(None)
        }
    }
}

fn command_requests_new_session(text: Option<&str>) -> bool {
    match text.map(str::trim) {
        None | Some("") | Some("start") => true,
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use core_service::SessionRequest;
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Clone, Default)]
    struct RecordingResolver {
        calls: Arc<Mutex<Vec<SessionId>>>,
        handle: Option<SessionHandle>,
    }

    #[derive(Clone, Default)]
    struct RecordingConfigurator {
        calls: Arc<Mutex<Vec<(SessionId, String)>>>,
    }

    #[async_trait]
    impl SessionRuntimeConfigurator for RecordingConfigurator {
        async fn register_project_root(&self, session_id: SessionId, project_root: &str) -> Result<()> {
            self.calls
                .lock()
                .await
                .push((session_id, project_root.to_string()));
            Ok(())
        }
    }

    #[async_trait]
    impl core_service::SessionRuntimeLiveness for RecordingConfigurator {
        async fn is_session_alive(&self, _session_id: SessionId) -> Result<bool> {
            Ok(true)
        }
    }

    #[derive(Clone, Default)]
    struct RecordingOrchestrator {
        started_channels: Arc<Mutex<Vec<String>>>,
        replies: Arc<Mutex<Vec<SlackThreadReply>>>,
        listed_channels: Arc<Mutex<Vec<String>>>,
        posted_lists: Arc<Mutex<Vec<(String, String)>>>,
        actions: Arc<Mutex<Vec<(String, String, SlackThreadAction)>>>,
    }

    #[async_trait]
    impl SessionHandleResolver for RecordingResolver {
        async fn resolve(&self, session_id: SessionId) -> Result<SessionHandle> {
            self.calls.lock().await.push(session_id);
            self.handle
                .clone()
                .ok_or_else(|| anyhow::anyhow!("missing session handle"))
        }
    }

    #[async_trait]
    impl SlackSessionOrchestrator for RecordingOrchestrator {
        async fn start_new_session(&self, channel_id: &str) -> Result<StartedSlackSession> {
            self.started_channels
                .lock()
                .await
                .push(channel_id.to_string());
            Ok(StartedSlackSession {
                session_id: SessionId::new(),
                state: SessionState::Starting,
                binding: TransportBinding {
                    project_space_id: channel_id.to_string(),
                    session_space_id: "1740.100".to_string(),
                },
            })
        }

        async fn handle_session_reply(&self, reply: SlackThreadReply) -> Result<SessionState> {
            self.replies.lock().await.push(reply);
            Ok(SessionState::Idle)
        }

        async fn list_channel_sessions(&self, channel_id: &str) -> Result<Vec<SlackListedSession>> {
            self.listed_channels
                .lock()
                .await
                .push(channel_id.to_string());
            Ok(vec![SlackListedSession {
                session_id: SessionId::new(),
                tmux_session_name: "session-1".to_string(),
                thread_ts: "1740.100".to_string(),
                project_label: "demo".to_string(),
                state: SessionState::Idle,
            }])
        }

        async fn post_session_list(&self, channel_id: &str, thread_ts: &str) -> Result<()> {
            self.posted_lists
                .lock()
                .await
                .push((channel_id.to_string(), thread_ts.to_string()));
            Ok(())
        }

        async fn handle_thread_action(
            &self,
            channel_id: &str,
            thread_ts: &str,
            action: SlackThreadAction,
        ) -> Result<Option<SessionState>> {
            self.actions.lock().await.push((
                channel_id.to_string(),
                thread_ts.to_string(),
                action,
            ));
            Ok(Some(SessionState::Idle))
        }
    }

    fn fake_handle(session_id: SessionId, state: SessionState) -> SessionHandle {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionRequest>(1);
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                let _ = request.reply_tx.send(Ok(state.clone()));
            }
        });

        SessionHandle::new_for_tests(session_id, sender)
    }

    #[derive(Clone, Default)]
    struct RecordingWorkingStatusPublisher {
        statuses: Arc<Mutex<Vec<(SlackMessageTarget, String)>>>,
    }

    #[async_trait]
    impl SlackWorkingStatusPublisher for RecordingWorkingStatusPublisher {
        async fn post_working_status(
            &self,
            target: &SlackMessageTarget,
            text: impl Into<String> + Send,
        ) -> Result<SlackThreadStatus> {
            let text = text.into();
            self.statuses
                .lock()
                .await
                .push((target.clone(), text.clone()));

            Ok(SlackThreadStatus {
                channel_id: target.channel_id.clone(),
                thread_ts: target.thread_ts.clone(),
                status_message_ts: "1740.200".to_string(),
            })
        }
    }

    #[derive(Clone, Default)]
    struct RecordingSessionPublisher {
        channel_messages: Arc<Mutex<Vec<(String, String)>>>,
        threaded_block_messages: Arc<Mutex<Vec<(SlackMessageTarget, String, usize)>>>,
        permalink_requests: Arc<Mutex<Vec<(String, String)>>>,
        status_updates: Arc<Mutex<Vec<(SlackThreadStatus, String)>>>,
        fail_updates: Arc<Mutex<bool>>,
        final_replies: Arc<Mutex<Vec<(SlackMessageTarget, String)>>>,
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
            self.threaded_block_messages.lock().await.push((
                target.clone(),
                text.to_string(),
                blocks.len(),
            ));

            Ok(SlackPostedMessage {
                channel_id: target.channel_id.clone(),
                message_ts: "1740.301".to_string(),
            })
        }

        async fn update_working_status(
            &self,
            status: &SlackThreadStatus,
            text: &str,
        ) -> Result<()> {
            if *self.fail_updates.lock().await {
                return Err(anyhow::anyhow!("Slack API error: message_not_found"));
            }
            self.status_updates
                .lock()
                .await
                .push((status.clone(), text.to_string()));
            Ok(())
        }

        async fn delete_message(&self, _status: &SlackThreadStatus) -> Result<()> {
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
                message_ts: "1740.300".to_string(),
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

    #[test]
    fn parse_thread_reply_accepts_normal_user_thread_message() {
        let parsed = parse_thread_reply(SlackEnvelope {
            channel: Some("C123".to_string()),
            text: Some("continue".to_string()),
            thread_ts: Some("1740.100".to_string()),
            user: Some("U123".to_string()),
            bot_id: None,
            subtype: None,
        });

        assert_eq!(
            parsed,
            Some(SlackThreadReply {
                channel_id: "C123".to_string(),
                thread_ts: "1740.100".to_string(),
                text: "continue".to_string(),
                user_id: "U123".to_string(),
            })
        );
    }

    #[test]
    fn parse_thread_reply_rejects_bot_message() {
        let parsed = parse_thread_reply(SlackEnvelope {
            channel: Some("C123".to_string()),
            text: Some("continue".to_string()),
            thread_ts: Some("1740.100".to_string()),
            user: Some("U123".to_string()),
            bot_id: Some("B123".to_string()),
            subtype: None,
        });

        assert_eq!(parsed, None);
    }

    #[test]
    fn command_requests_new_session_accepts_empty_and_start() {
        assert!(command_requests_new_session(None));
        assert!(command_requests_new_session(Some("")));
        assert!(command_requests_new_session(Some("start")));
        assert!(!command_requests_new_session(Some("command")));
    }

    #[test]
    fn parse_socket_mode_request_reads_slash_command_envelope() {
        let parsed = parse_socket_mode_request(
            r#"{
              "envelope_id":"env-1",
              "type":"slash_commands",
              "accepts_response_payload":true,
              "payload":{
                "command":"/cc",
                "channel_id":"C123",
                "user_id":"U123",
                "text":"start"
              }
            }"#,
        )
        .expect("parse socket mode request");

        assert_eq!(
            parsed,
            SocketModeRequest::SlashCommand {
                envelope_id: "env-1".to_string(),
                payload: SlackSlashCommandPayload {
                    command: "/cc".to_string(),
                    channel_id: "C123".to_string(),
                    user_id: "U123".to_string(),
                    text: Some("start".to_string()),
                },
            }
        );
    }

    #[test]
    fn build_socket_mode_ack_includes_optional_payload() {
        let ack = build_socket_mode_ack("env-1", Some(json!({ "text": "Starting..." })))
            .expect("build ack");
        let payload: serde_json::Value = serde_json::from_str(&ack).expect("parse ack");

        assert_eq!(payload["envelope_id"], "env-1");
        assert_eq!(payload["payload"]["text"], "Starting...");
    }

    #[test]
    fn build_main_menu_response_matches_slack_entrypoint_contract() {
        let payload = build_main_menu_response();

        assert_eq!(payload["text"], "Choose an action");
        assert_eq!(payload["blocks"][0]["type"], "actions");
        assert_eq!(payload["blocks"][0]["elements"][0]["action_id"], "claude_session_new");
        assert_eq!(payload["blocks"][0]["elements"][1]["action_id"], "claude_session_list");
    }

    #[test]
    fn parse_socket_mode_request_reads_block_action_envelope() {
        let parsed = parse_socket_mode_request(
            r#"{
              "envelope_id":"env-2",
              "type":"interactive",
              "payload":{
                "type":"block_actions",
                "channel":{"id":"C123"},
                "container":{"channel_id":"C123","message_ts":"1740.900"},
                "actions":[{"action_id":"claude_session_new","value":"claude.session.new"}]
              }
            }"#,
        )
        .expect("parse interactive request");

        assert_eq!(
            parsed,
            SocketModeRequest::Interactive {
                envelope_id: "env-2".to_string(),
                action: Some(SlackBlockActionPayload {
                    channel_id: "C123".to_string(),
                    thread_ts: Some("1740.900".to_string()),
                    action_id: "claude_session_new".to_string(),
                    value: Some("claude.session.new".to_string()),
                    user_id: None,
                }),
            }
        );
    }

    #[tokio::test]
    async fn interactive_new_session_action_starts_session_for_channel() {
        let orchestrator = Arc::new(RecordingOrchestrator::default());

        let ack = handle_socket_mode_text(
            orchestrator.clone(),
            &["U123".to_string()],
            r#"{
              "envelope_id":"env-2",
              "type":"interactive",
              "payload":{
                "type":"block_actions",
                "user":{"id":"U123"},
                "channel":{"id":"C123"},
                "container":{"channel_id":"C123","message_ts":"1740.900"},
                "actions":[{"action_id":"claude_session_new","value":"claude.session.new"}]
              }
            }"#,
        )
        .await
        .expect("handle interactive request");

        tokio::task::yield_now().await;

        let ack = ack.expect("interactive request should be acked");
        let payload: Value = serde_json::from_str(&ack).expect("parse ack");
        assert_eq!(payload["envelope_id"], "env-2");
        assert_eq!(
            orchestrator.started_channels.lock().await.as_slice(),
            &["C123".to_string()]
        );
    }

    #[tokio::test]
    async fn interactive_session_list_action_only_acks() {
        let orchestrator = Arc::new(RecordingOrchestrator::default());

        let ack = handle_socket_mode_text(
            orchestrator.clone(),
            &["U123".to_string()],
            r#"{
              "envelope_id":"env-3",
              "type":"interactive",
              "payload":{
                "type":"block_actions",
                "user":{"id":"U123"},
                "channel":{"id":"C123"},
                "container":{"channel_id":"C123","message_ts":"1740.901"},
                "actions":[{"action_id":"claude_session_list","value":"claude.session.list"}]
              }
            }"#,
        )
        .await
        .expect("handle interactive request");

        tokio::task::yield_now().await;

        let ack = ack.expect("interactive request should be acked");
        let payload: Value = serde_json::from_str(&ack).expect("parse ack");
        assert_eq!(payload["envelope_id"], "env-3");
        assert!(orchestrator.started_channels.lock().await.is_empty());
        assert!(payload.get("payload").is_none());
        assert_eq!(
            orchestrator.posted_lists.lock().await.as_slice(),
            &[("C123".to_string(), "1740.901".to_string())]
        );
    }

    #[tokio::test]
    async fn interactive_command_palette_action_routes_to_thread_action() {
        let orchestrator = Arc::new(RecordingOrchestrator::default());

        let ack = handle_socket_mode_text(
            orchestrator.clone(),
            &["U123".to_string()],
            r#"{
              "envelope_id":"env-4",
              "type":"interactive",
              "payload":{
                "type":"block_actions",
                "user":{"id":"U123"},
                "channel":{"id":"C123"},
                "container":{"channel_id":"C123","message_ts":"1740.902"},
                "actions":[{"action_id":"claude_command_palette_open","value":"open"}]
              }
            }"#,
        )
        .await
        .expect("handle interactive request");

        let ack = ack.expect("interactive request should be acked");
        let payload: Value = serde_json::from_str(&ack).expect("parse ack");
        assert_eq!(payload["envelope_id"], "env-4");
        assert_eq!(
            orchestrator.actions.lock().await.as_slice(),
            &[(
                "C123".to_string(),
                "1740.902".to_string(),
                SlackThreadAction::OpenCommandPalette,
            )]
        );
    }

    #[tokio::test]
    async fn interactive_open_thread_url_action_is_treated_as_noop() {
        let orchestrator = Arc::new(RecordingOrchestrator::default());

        let ack = handle_socket_mode_text(
            orchestrator.clone(),
            &[],
            r#"{
              "envelope_id":"env-5",
              "type":"interactive",
              "payload":{
                "type":"block_actions",
                "channel":{"id":"C123"},
                "container":{"channel_id":"C123","message_ts":"1740.903"},
                "actions":[{"action_id":"claude_session_open_thread"}]
              }
            }"#,
        )
        .await
        .expect("handle interactive request");

        let ack = ack.expect("interactive request should be acked");
        let payload: Value = serde_json::from_str(&ack).expect("parse ack");
        assert_eq!(payload["envelope_id"], "env-5");
        assert!(orchestrator.actions.lock().await.is_empty());
        assert!(orchestrator.started_channels.lock().await.is_empty());
        assert!(orchestrator.posted_lists.lock().await.is_empty());
    }

    #[test]
    fn build_thread_message_request_targets_thread() {
        let request = build_thread_message_request(
            &SlackMessageTarget {
                channel_id: "C123".to_string(),
                thread_ts: "1740.100".to_string(),
            },
            "working",
        );

        assert_eq!(request.channel, SlackChannelId("C123".into()));
        assert_eq!(request.thread_ts, Some(SlackTs("1740.100".into())));
        assert_eq!(request.content.text, Some("working".to_string()));
    }

    #[test]
    fn build_status_update_request_targets_existing_message() {
        let request = build_status_update_request(
            &SlackPostedMessage {
                channel_id: "C123".to_string(),
                message_ts: "1740.200".to_string(),
            },
            "done",
        );

        assert_eq!(request.channel, SlackChannelId("C123".into()));
        assert_eq!(request.ts, SlackTs("1740.200".into()));
        assert_eq!(request.content.text, Some("done".to_string()));
    }

    #[test]
    fn build_status_delete_request_targets_existing_message() {
        let request = build_status_delete_request(&SlackThreadStatus {
            channel_id: "C123".to_string(),
            thread_ts: "1740.100".to_string(),
            status_message_ts: "1740.200".to_string(),
        });

        assert_eq!(request.channel, SlackChannelId("C123".into()));
        assert_eq!(request.ts, SlackTs("1740.200".into()));
    }

    #[test]
    fn slack_thread_status_keeps_thread_and_status_message_identity() {
        let status = SlackThreadStatus {
            channel_id: "C123".to_string(),
            thread_ts: "1740.100".to_string(),
            status_message_ts: "1740.200".to_string(),
        };

        assert_eq!(status.channel_id, "C123");
        assert_eq!(status.thread_ts, "1740.100");
        assert_eq!(status.status_message_ts, "1740.200");
    }

    #[test]
    fn parse_push_thread_reply_accepts_user_message_in_thread() {
        let parsed = parse_push_thread_reply(&SlackPushEventCallback {
            team_id: SlackTeamId("T123".into()),
            api_app_id: SlackAppId("A123".into()),
            event: SlackEventCallbackBody::Message(SlackMessageEvent {
                origin: SlackMessageOrigin {
                    ts: SlackTs("1740.200".into()),
                    channel: Some(SlackChannelId("C123".into())),
                    channel_type: None,
                    thread_ts: Some(SlackTs("1740.100".into())),
                    client_msg_id: None,
                },
                content: Some(SlackMessageContent {
                    text: Some("continue".into()),
                    blocks: None,
                    attachments: None,
                    upload: None,
                    files: None,
                    reactions: None,
                    metadata: None,
                }),
                sender: SlackMessageSender {
                    user: Some(SlackUserId("U123".into())),
                    bot_id: None,
                    username: None,
                    display_as_bot: None,
                    user_profile: None,
                    bot_profile: None,
                },
                subtype: None,
                hidden: None,
                message: None,
                previous_message: None,
                deleted_ts: None,
            }),
            event_id: SlackEventId("Ev123".into()),
            event_time: SlackDateTime(Utc::now()),
            event_context: None,
            authed_users: None,
            authorizations: None,
        });

        assert_eq!(
            parsed,
            Some(SlackThreadReply {
                channel_id: "C123".to_string(),
                thread_ts: "1740.100".to_string(),
                text: "continue".to_string(),
                user_id: "U123".to_string(),
            })
        );
    }

    #[test]
    fn parse_push_thread_reply_rejects_message_without_top_level_text() {
        let parsed = parse_push_thread_reply(&SlackPushEventCallback {
            team_id: SlackTeamId("T123".into()),
            api_app_id: SlackAppId("A123".into()),
            event: SlackEventCallbackBody::Message(SlackMessageEvent {
                origin: SlackMessageOrigin {
                    ts: SlackTs("1740.200".into()),
                    channel: Some(SlackChannelId("C123".into())),
                    channel_type: None,
                    thread_ts: Some(SlackTs("1740.100".into())),
                    client_msg_id: None,
                },
                content: Some(SlackMessageContent {
                    text: None,
                    blocks: None,
                    attachments: None,
                    upload: None,
                    files: None,
                    reactions: None,
                    metadata: None,
                }),
                sender: SlackMessageSender {
                    user: Some(SlackUserId("U123".into())),
                    bot_id: None,
                    username: None,
                    display_as_bot: None,
                    user_profile: None,
                    bot_profile: None,
                },
                subtype: None,
                hidden: None,
                message: Some(SlackMessageEventEdited {
                    ts: SlackTs("1740.200".into()),
                    content: Some(SlackMessageContent::new().with_text("continue".to_string())),
                    sender: SlackMessageSender {
                        user: Some(SlackUserId("U123".into())),
                        bot_id: None,
                        username: None,
                        display_as_bot: None,
                        user_profile: None,
                        bot_profile: None,
                    },
                    edited: None,
                }),
                previous_message: None,
                deleted_ts: None,
            }),
            event_id: SlackEventId("Ev123".into()),
            event_time: SlackDateTime(Utc::now()),
            event_context: None,
            authed_users: None,
            authorizations: None,
        });

        assert_eq!(parsed, None);
    }

    #[tokio::test]
    async fn handle_thread_reply_routes_text_to_bound_session() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let session_id = SessionId::new();
        store
            .insert(
                TransportBinding {
                    project_space_id: "C123".to_string(),
                    session_space_id: "1740.100".to_string(),
                },
                session_id,
            )
            .await;
        let resolver = Arc::new(RecordingResolver {
            calls: Arc::new(Mutex::new(Vec::new())),
            handle: Some(fake_handle(session_id, SessionState::Idle)),
        });
        let transport = SlackTransport::new(store, resolver.clone(), Arc::new(RecordingConfigurator::default()));

        let state = transport
            .handle_thread_reply(SlackThreadReply {
                channel_id: "C123".to_string(),
                thread_ts: "1740.100".to_string(),
                text: "continue".to_string(),
                user_id: "U123".to_string(),
            })
            .await
            .expect("route thread reply");

        assert_eq!(state, SessionState::Idle);
        assert_eq!(*resolver.calls.lock().await, vec![session_id]);
    }

    #[tokio::test]
    async fn handle_thread_reply_errors_when_binding_is_missing() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let resolver = Arc::new(RecordingResolver::default());
        let transport = SlackTransport::new(store, resolver, Arc::new(RecordingConfigurator::default()));

        let error = transport
            .handle_thread_reply(SlackThreadReply {
                channel_id: "C123".to_string(),
                thread_ts: "1740.100".to_string(),
                text: "continue".to_string(),
                user_id: "U123".to_string(),
            })
            .await
            .expect_err("missing binding should fail");

        assert!(error.to_string().contains("no session binding"));
    }

    #[tokio::test]
    async fn bind_thread_persists_binding_for_future_lookup() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let session_id = SessionId::new();
        let resolver = Arc::new(RecordingResolver {
            calls: Arc::new(Mutex::new(Vec::new())),
            handle: Some(fake_handle(session_id, SessionState::Idle)),
        });
        let transport = SlackTransport::new(
            store.clone(),
            resolver,
            Arc::new(RecordingConfigurator::default()),
        );

        transport
            .bind_thread("C999", "2000.100", session_id)
            .await
            .expect("bind thread");

        let loaded = store
            .find_session_id(&TransportBinding {
                project_space_id: "C999".to_string(),
                session_space_id: "2000.100".to_string(),
            })
            .await
            .expect("load binding");

        assert_eq!(loaded, Some(session_id));
    }

    #[tokio::test]
    async fn start_session_binds_thread_and_initializes_idle_state() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let session_id = SessionId::new();
        let resolver = Arc::new(RecordingResolver {
            calls: Arc::new(Mutex::new(Vec::new())),
            handle: Some(fake_handle(session_id, SessionState::Idle)),
        });
        let configurator = Arc::new(RecordingConfigurator::default());
        let transport = SlackTransport::new(store.clone(), resolver.clone(), configurator.clone());

        let started = transport
            .start_session(SlackSessionStart {
                channel_id: "C777".to_string(),
                thread_ts: "3000.100".to_string(),
            }, "/tmp/project")
            .await
            .expect("start session");

        let loaded = store
            .find_session_id(&TransportBinding {
                project_space_id: "C777".to_string(),
                session_space_id: "3000.100".to_string(),
            })
            .await
            .expect("load binding");

        assert_eq!(loaded, Some(started.session_id));
        assert_eq!(started.state, SessionState::Idle);
        assert_eq!(*resolver.calls.lock().await, vec![started.session_id]);
        assert_eq!(
            configurator.calls.lock().await.as_slice(),
            &[(started.session_id, "/tmp/project".to_string())]
        );
    }

    #[tokio::test]
    async fn start_session_with_working_status_persists_status_message_binding() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let resolver = Arc::new(RecordingResolver {
            calls: Arc::new(Mutex::new(Vec::new())),
            handle: Some(fake_handle(SessionId::new(), SessionState::Idle)),
        });
        let publisher = RecordingWorkingStatusPublisher::default();
        let transport = SlackTransport::new(
            store.clone(),
            resolver,
            Arc::new(RecordingConfigurator::default()),
        );

        let started = transport
            .start_session_with_working_status(
                SlackSessionStart {
                    channel_id: "C777".to_string(),
                    thread_ts: "3000.100".to_string(),
                },
                "/tmp/project",
                &publisher,
            )
            .await
            .expect("start session with working status");

        let persisted = store
            .find_status_message(&started.binding)
            .await
            .expect("find status message")
            .expect("status message should exist");

        assert_eq!(persisted.status_message_id, "1740.200");
        assert_eq!(
            publisher.statuses.lock().await.as_slice(),
            &[(
                SlackMessageTarget {
                    channel_id: "C777".to_string(),
                    thread_ts: "3000.100".to_string(),
                },
                "⏳ Working...".to_string(),
            )]
        );
    }

    #[tokio::test]
    async fn update_working_status_uses_persisted_status_message_identity() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let binding = TransportBinding {
            project_space_id: "C777".to_string(),
            session_space_id: "3000.100".to_string(),
        };
        store
            .save_status_message(&TransportStatusMessage {
                binding: binding.clone(),
                status_message_id: "3000.200".to_string(),
            })
            .await
            .expect("save status message");
        let transport = SlackTransport::new(
            store,
            Arc::new(RecordingResolver::default()),
            Arc::new(RecordingConfigurator::default()),
        );
        let publisher = RecordingSessionPublisher::default();

        transport
            .update_working_status(&binding, &publisher, "Still working...")
            .await
            .expect("update working status");

        assert_eq!(
            publisher.status_updates.lock().await.as_slice(),
            &[(
                SlackThreadStatus {
                    channel_id: "C777".to_string(),
                    thread_ts: "3000.100".to_string(),
                    status_message_ts: "3000.200".to_string(),
                },
                "Still working...".to_string(),
            )]
        );
    }

    #[tokio::test]
    async fn ensure_working_status_reposts_when_prior_status_was_deleted() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let binding = TransportBinding {
            project_space_id: "C777".to_string(),
            session_space_id: "3000.100".to_string(),
        };
        store
            .save_status_message(&TransportStatusMessage {
                binding: binding.clone(),
                status_message_id: "3000.200".to_string(),
            })
            .await
            .expect("save status message");
        let transport = SlackTransport::new(
            store.clone(),
            Arc::new(RecordingResolver::default()),
            Arc::new(RecordingConfigurator::default()),
        );
        let publisher = RecordingWorkingStatusPublisher::default();
        let session_publisher = RecordingSessionPublisher::default();
        *session_publisher.fail_updates.lock().await = true;

        struct CombinedPublisher {
            status: RecordingWorkingStatusPublisher,
            session: RecordingSessionPublisher,
        }

        #[async_trait]
        impl SlackWorkingStatusPublisher for CombinedPublisher {
            async fn post_working_status(
                &self,
                target: &SlackMessageTarget,
                text: impl Into<String> + Send,
            ) -> Result<SlackThreadStatus> {
                self.status.post_working_status(target, text).await
            }
        }

        #[async_trait]
        impl SlackSessionPublisher for CombinedPublisher {
            async fn post_channel_message(&self, channel_id: &str, text: &str) -> Result<SlackPostedMessage> {
                self.session.post_channel_message(channel_id, text).await
            }

            async fn post_thread_message_with_blocks(
                &self,
                target: &SlackMessageTarget,
                text: &str,
                blocks: Vec<SlackBlock>,
            ) -> Result<SlackPostedMessage> {
                self.session
                    .post_thread_message_with_blocks(target, text, blocks)
                    .await
            }

            async fn update_working_status(&self, status: &SlackThreadStatus, text: &str) -> Result<()> {
                self.session.update_working_status(status, text).await
            }

            async fn delete_message(&self, status: &SlackThreadStatus) -> Result<()> {
                self.session.delete_message(status).await
            }

            async fn get_message_permalink(&self, channel_id: &str, message_ts: &str) -> Result<String> {
                self.session.get_message_permalink(channel_id, message_ts).await
            }

            async fn post_final_reply(
                &self,
                target: &SlackMessageTarget,
                text: &str,
            ) -> Result<SlackPostedMessage> {
                self.session.post_final_reply(target, text).await
            }
        }

        let publisher = CombinedPublisher {
            status: publisher.clone(),
            session: session_publisher.clone(),
        };

        transport
            .ensure_working_status(&binding, &publisher, "⏳ Working...")
            .await
            .expect("ensure working status");

        assert_eq!(
            publisher.status.statuses.lock().await.as_slice(),
            &[(
                SlackMessageTarget {
                    channel_id: "C777".to_string(),
                    thread_ts: "3000.100".to_string(),
                },
                "⏳ Working...".to_string(),
            )]
        );
    }

    #[tokio::test]
    async fn post_final_reply_targets_bound_thread() {
        let store = Arc::new(InMemorySlackBindingStore::new());
        let binding = TransportBinding {
            project_space_id: "C777".to_string(),
            session_space_id: "3000.100".to_string(),
        };
        let transport = SlackTransport::new(
            store,
            Arc::new(RecordingResolver::default()),
            Arc::new(RecordingConfigurator::default()),
        );
        let publisher = RecordingSessionPublisher::default();

        transport
            .post_final_reply(&binding, &publisher, "Finished.")
            .await
            .expect("post final reply");

        assert_eq!(
            publisher.final_replies.lock().await.as_slice(),
            &[(
                SlackMessageTarget {
                    channel_id: "C777".to_string(),
                    thread_ts: "3000.100".to_string(),
                },
                "Finished.".to_string(),
            )]
        );
    }

    #[test]
    fn format_claude_text_for_slack_normalizes_markdown_for_readability() {
        let messages = format_claude_text_for_slack(
            "# 요약\n\n**중요**\n\n| 항목 | 상태 |\n| --- | --- |\n| 테스트 | 통과 |\n",
        );

        assert_eq!(messages.len(), 1);
        assert!(messages[0].text.contains("요약"));
        assert!(messages[0].text.contains("중요"));
        assert!(!messages[0].text.contains("**중요**"));
        assert!(messages[0].text.contains("| 항목 | 상태 |"));
    }

    #[test]
    fn format_claude_text_for_slack_splits_long_messages() {
        let source = format!("{}\n\n{}", "a".repeat(2_500), "b".repeat(200));
        let messages = format_claude_text_for_slack(&source);

        assert_eq!(messages.len(), 2);
        assert!(messages[0].text.chars().count() <= 2_500);
        assert_eq!(messages[1].text, "b".repeat(200));
    }

    #[test]
    fn format_claude_text_for_slack_strips_code_fences_for_plain_delivery() {
        let messages = format_claude_text_for_slack("**중요**\n\n```\n**코드**\n```");

        assert_eq!(messages.len(), 1);
        assert!(messages[0].text.contains("중요"));
        assert!(messages[0].text.contains("코드"));
        assert!(!messages[0].text.contains("```"));
    }

    #[test]
    fn build_thread_message_request_with_blocks_supports_markdown_blocks() {
        let target = SlackMessageTarget {
            channel_id: "C777".to_string(),
            thread_ts: "3000.100".to_string(),
        };
        let request = build_thread_message_request_with_blocks(
            &target,
            "Fallback text",
            vec![SlackMarkdownBlock {
                block_id: None,
                text: "**Bold**\n\n1. First".to_string(),
            }
            .into()],
        );

        let payload = serde_json::to_value(&request).expect("serialize request");

        assert_eq!(payload["blocks"][0]["type"], "markdown");
        assert_eq!(payload["blocks"][0]["text"], "**Bold**\n\n1. First");
        assert_eq!(payload["text"], "Fallback text");
    }

    #[test]
    fn to_plain_fallback_strips_slack_sensitive_markdown() {
        let text = to_plain_fallback("# Summary\n\n**Bold**\n\n`code`");

        assert_eq!(text, "Summary\n\nBold\n\ncode");
    }

    #[test]
    fn thread_reply_carries_sender_user_id() {
        let parsed = parse_thread_reply(SlackEnvelope {
            channel: Some("C123".to_string()),
            text: Some("hello".to_string()),
            thread_ts: Some("1740.100".to_string()),
            user: Some("U456".to_string()),
            bot_id: None,
            subtype: None,
        });
        assert_eq!(parsed.unwrap().user_id, "U456");
    }

    #[test]
    fn thread_reply_from_non_allowed_user_is_blocked_by_is_allowed_user() {
        let reply = parse_thread_reply(SlackEnvelope {
            channel: Some("C123".to_string()),
            text: Some("hello".to_string()),
            thread_ts: Some("1740.100".to_string()),
            user: Some("U123".to_string()),
            bot_id: None,
            subtype: None,
        })
        .unwrap();

        let allowed = vec!["U999".to_string()];
        assert!(!is_allowed_user(&reply.user_id, &allowed));

        let allowed_with_user = vec!["U123".to_string()];
        assert!(is_allowed_user(&reply.user_id, &allowed_with_user));
    }

    #[test]
    fn parse_allowed_user_ids_returns_empty_for_blank_value() {
        assert!(parse_allowed_user_ids("").is_empty());
        assert!(parse_allowed_user_ids("  , , ").is_empty());
    }

    #[test]
    fn parse_allowed_user_ids_parses_single_id() {
        assert_eq!(parse_allowed_user_ids("U123"), vec!["U123".to_string()]);
    }

    #[test]
    fn parse_allowed_user_ids_parses_multiple_ids() {
        assert_eq!(
            parse_allowed_user_ids("U123,U456,U789"),
            vec!["U123".to_string(), "U456".to_string(), "U789".to_string()]
        );
    }

    #[test]
    fn parse_allowed_user_ids_trims_whitespace_and_skips_empty() {
        assert_eq!(
            parse_allowed_user_ids(" U123 , U456 , , U789 "),
            vec!["U123".to_string(), "U456".to_string(), "U789".to_string()]
        );
    }

    #[test]
    fn is_allowed_user_denies_all_when_list_is_empty() {
        // Fail-closed: empty allowlist must deny everyone.
        assert!(!is_allowed_user("U123", &[]));
        assert!(!is_allowed_user("", &[]));
    }

    #[test]
    fn is_allowed_user_matches_exact_id() {
        let ids = vec!["U123".to_string(), "U456".to_string()];
        assert!(is_allowed_user("U123", &ids));
        assert!(is_allowed_user("U456", &ids));
        assert!(!is_allowed_user("U999", &ids));
    }

    #[tokio::test]
    async fn slash_command_from_non_allowed_user_returns_unauthorized_ack() {
        let orchestrator = Arc::new(RecordingOrchestrator::default());
        let allowed = vec!["U999".to_string()];

        let ack = handle_socket_mode_text(
            orchestrator.clone(),
            &allowed,
            r#"{
              "envelope_id":"env-1",
              "type":"slash_commands",
              "accepts_response_payload":true,
              "payload":{
                "command":"/cc",
                "channel_id":"C123",
                "user_id":"U123",
                "text":"start"
              }
            }"#,
        )
        .await
        .expect("handle slash command");

        tokio::task::yield_now().await;

        let ack = ack.expect("slash command should be acked");
        let payload: Value = serde_json::from_str(&ack).expect("parse ack");
        assert_eq!(payload["envelope_id"], "env-1");
        assert!(
            payload["payload"]["text"]
                .as_str()
                .unwrap_or("")
                .contains("not authorized"),
            "ack payload should contain 'not authorized'"
        );
        assert!(
            orchestrator.started_channels.lock().await.is_empty(),
            "no session should be started for non-allowed user"
        );
    }

    #[tokio::test]
    async fn interactive_action_from_non_allowed_user_is_acked_but_not_processed() {
        let orchestrator = Arc::new(RecordingOrchestrator::default());
        let allowed = vec!["U999".to_string()];

        let ack = handle_socket_mode_text(
            orchestrator.clone(),
            &allowed,
            r#"{
              "envelope_id":"env-2",
              "type":"interactive",
              "payload":{
                "type":"block_actions",
                "user":{"id":"U123"},
                "channel":{"id":"C123"},
                "container":{"channel_id":"C123","message_ts":"1740.900"},
                "actions":[{"action_id":"claude_session_new","value":"claude.session.new"}]
              }
            }"#,
        )
        .await
        .expect("handle interactive request");

        tokio::task::yield_now().await;

        let ack = ack.expect("interactive request should be acked");
        let payload: Value = serde_json::from_str(&ack).expect("parse ack");
        assert_eq!(payload["envelope_id"], "env-2");
        assert!(
            orchestrator.started_channels.lock().await.is_empty(),
            "no session should be started for non-allowed user"
        );
    }

    // ── Scenario: 허용된 사용자의 슬래시 커맨드 ──────────────────────────────────
    //
    // Scenario: 허용된 사용자가 /cc start를 보내면 세션이 시작된다
    //   Given 허용 목록에 U123이 등록되어 있다
    //   When U123이 /cc start 슬래시 커맨드를 보낸다
    //   Then 해당 채널에 세션 시작이 요청된다
    #[tokio::test]
    async fn 허용된_사용자의_슬래시_커맨드는_세션을_시작한다() {
        // Given
        let orchestrator = Arc::new(RecordingOrchestrator::default());
        let allowed = vec!["U123".to_string()];

        // When
        let ack = handle_socket_mode_text(
            orchestrator.clone(),
            &allowed,
            r#"{
              "envelope_id":"env-allowed-1",
              "type":"slash_commands",
              "accepts_response_payload":true,
              "payload":{
                "command":"/cc",
                "channel_id":"C123",
                "user_id":"U123",
                "text":"start"
              }
            }"#,
        )
        .await
        .expect("handle slash command");

        tokio::task::yield_now().await;

        // Then
        let ack = ack.expect("slash command should be acked");
        let payload: Value = serde_json::from_str(&ack).expect("parse ack");
        assert_eq!(payload["envelope_id"], "env-allowed-1");
        assert_eq!(
            orchestrator.started_channels.lock().await.as_slice(),
            &["C123".to_string()],
            "허용된 사용자의 커맨드는 세션을 시작해야 함"
        );
    }

    // ── Scenario: 다중 허용 사용자 ───────────────────────────────────────────────
    //
    // Scenario: 여러 허용 사용자 중 하나가 커맨드를 보내도 세션이 시작된다
    //   Given 허용 목록에 U123, U456, U789가 등록되어 있다
    //   When U456이 /cc start 슬래시 커맨드를 보낸다
    //   Then 세션 시작이 요청된다
    //   And U789가 /cc start를 보내도 세션이 시작된다
    #[tokio::test]
    async fn 다중_허용_사용자_중_누구나_세션을_시작할_수_있다() {
        // Given
        let orchestrator = Arc::new(RecordingOrchestrator::default());
        let allowed = vec!["U123".to_string(), "U456".to_string(), "U789".to_string()];

        // When - 두 번째 허용 사용자
        handle_socket_mode_text(
            orchestrator.clone(),
            &allowed,
            r#"{
              "envelope_id":"env-multi-1",
              "type":"slash_commands",
              "accepts_response_payload":true,
              "payload":{
                "command":"/cc",
                "channel_id":"C456",
                "user_id":"U456",
                "text":"start"
              }
            }"#,
        )
        .await
        .expect("handle U456 slash command");
        tokio::task::yield_now().await;

        // When - 세 번째 허용 사용자
        handle_socket_mode_text(
            orchestrator.clone(),
            &allowed,
            r#"{
              "envelope_id":"env-multi-2",
              "type":"slash_commands",
              "accepts_response_payload":true,
              "payload":{
                "command":"/cc",
                "channel_id":"C789",
                "user_id":"U789",
                "text":"start"
              }
            }"#,
        )
        .await
        .expect("handle U789 slash command");
        tokio::task::yield_now().await;

        // Then - 두 채널 모두 세션 시작
        let started = orchestrator.started_channels.lock().await;
        assert!(started.contains(&"C456".to_string()), "U456의 채널 세션이 시작되어야 함");
        assert!(started.contains(&"C789".to_string()), "U789의 채널 세션이 시작되어야 함");
    }

    // ── Scenario: 허용된 사용자의 interactive action ─────────────────────────────
    //
    // Scenario: 허용된 사용자가 세션 시작 버튼을 누르면 세션이 시작된다
    //   Given 허용 목록에 U123이 등록되어 있다
    //   When U123이 claude_session_new 인터랙션을 보낸다
    //   Then 해당 채널에 세션 시작이 요청된다
    #[tokio::test]
    async fn 허용된_사용자의_interactive_action은_처리된다() {
        // Given
        let orchestrator = Arc::new(RecordingOrchestrator::default());
        let allowed = vec!["U123".to_string()];

        // When
        let ack = handle_socket_mode_text(
            orchestrator.clone(),
            &allowed,
            r#"{
              "envelope_id":"env-allowed-2",
              "type":"interactive",
              "payload":{
                "type":"block_actions",
                "user":{"id":"U123"},
                "channel":{"id":"C123"},
                "container":{"channel_id":"C123","message_ts":"1740.900"},
                "actions":[{"action_id":"claude_session_new","value":"claude.session.new"}]
              }
            }"#,
        )
        .await
        .expect("handle interactive request");
        tokio::task::yield_now().await;

        // Then
        let ack = ack.expect("interactive request should be acked");
        let payload: Value = serde_json::from_str(&ack).expect("parse ack");
        assert_eq!(payload["envelope_id"], "env-allowed-2");
        assert_eq!(
            orchestrator.started_channels.lock().await.as_slice(),
            &["C123".to_string()],
            "허용된 사용자의 인터랙션은 세션을 시작해야 함"
        );
    }

}
