use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    process::Stdio,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use core_model::{SessionId, SessionMsg, SessionState, TurnId};
use core_service::{
    RuntimeEngine, SessionMessageSink, SessionRuntimeCleanup, SessionRuntimeConfigurator,
    SessionRuntimeLiveness,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{fs, process::Command, task::JoinHandle, time::sleep};
use tokio::sync::{Mutex, OnceCell};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRelayEvent {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub event: HookRelayEventKind,
    pub text: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookRelayEventKind {
    Stop,
    StopFailure,
    Notification,
    PreToolUse,
    PostToolUse,
}

pub async fn read_hook_events(file_path: &str) -> anyhow::Result<Vec<HookRelayEvent>> {
    let raw = match fs::read_to_string(file_path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(anyhow::Error::from))
        .collect()
}

pub fn pick_undelivered_terminal_events(
    last_delivered_turn_id: Option<&str>,
    events: &[HookRelayEvent],
) -> Vec<HookRelayEvent> {
    let terminal_events: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                HookRelayEventKind::Stop
                    | HookRelayEventKind::StopFailure
                    | HookRelayEventKind::Notification
            )
        })
        .cloned()
        .collect();

    let Some(last_delivered_turn_id) = last_delivered_turn_id else {
        return terminal_events;
    };

    let Some(index) = terminal_events
        .iter()
        .position(|event| event.turn_id == last_delivered_turn_id)
    else {
        return terminal_events;
    };

    terminal_events.into_iter().skip(index + 1).collect()
}

fn is_assistant_terminal_event(event: &HookRelayEvent) -> bool {
    matches!(event.event, HookRelayEventKind::Stop | HookRelayEventKind::StopFailure)
}

fn extract_claude_session_id(
    session_id: SessionId,
    last_delivered_turn_id: Option<&str>,
    events: &[HookRelayEvent],
) -> Option<String> {
    let latest_terminal_turn_id = events
        .iter()
        .rev()
        .find(|event| matches!(event.event, HookRelayEventKind::Stop | HookRelayEventKind::StopFailure | HookRelayEventKind::Notification))
        .map(|event| event.turn_id.as_str());
    let latest_turn_id = events
        .last()
        .map(|event| event.turn_id.as_str())
        .or(latest_terminal_turn_id)
        .or(last_delivered_turn_id)?;

    latest_turn_id
        .split(':')
        .next()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| Some(session_id.0.to_string()))
}

fn build_project_session_log_path(project_root: &str, claude_session_id: &str) -> PathBuf {
    let encoded_project_path = project_root.replace('/', "-");
    PathBuf::from(env::var("HOME").unwrap_or_default())
        .join(".claude")
        .join("projects")
        .join(encoded_project_path)
        .join(format!("{claude_session_id}.jsonl"))
}

async fn read_last_assistant_text_from_transcript(transcript_path: &PathBuf) -> anyhow::Result<String> {
    let raw = match fs::read_to_string(transcript_path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error.into()),
    };

    for line in raw.lines().rev().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if entry
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            != Some("assistant")
        {
            continue;
        }

        let Some(content) = entry
            .get("message")
            .and_then(|message| message.get("content"))
        else {
            continue;
        };

        let text = if let Some(text) = content.as_str() {
            text.trim().to_string()
        } else if let Some(items) = content.as_array() {
            items
                .iter()
                .filter_map(|item| {
                    (item.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| item.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        } else {
            String::new()
        };

        if !text.is_empty() {
            return Ok(text);
        }
    }

    Ok(String::new())
}

fn get_idle_prompt_input(pane: &str) -> Option<String> {
    let lines: Vec<String> = pane
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect();

    for (index, line) in lines.iter().enumerate().rev() {
        let Some(captures) = line.strip_prefix('❯') else {
            continue;
        };
        if index + 6 < lines.len() {
            return None;
        }
        return Some(captures.trim().to_string());
    }

    None
}

fn synthetic_stop_event(session_id: SessionId, claude_session_id: &str, text: String) -> HookRelayEvent {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    HookRelayEvent {
        session_id: session_id.0.to_string(),
        turn_id: format!("{claude_session_id}:{millis}"),
        event: HookRelayEventKind::Stop,
        text,
        created_at: millis.to_string(),
    }
}

pub fn pick_latest_progress_event(
    last_delivered_turn_id: Option<&str>,
    events: &[HookRelayEvent],
) -> Option<HookRelayEvent> {
    let start_index = last_delivered_turn_id
        .and_then(|turn_id| events.iter().position(|event| event.turn_id == turn_id))
        .map(|index| index + 1)
        .unwrap_or(0);

    events[start_index..]
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                HookRelayEventKind::PreToolUse | HookRelayEventKind::PostToolUse
            )
        })
        .cloned()
        .last()
}

struct LocalRuntimeState {
    event_sink: OnceCell<Arc<dyn SessionMessageSink>>,
    pending_turns: Mutex<HashMap<SessionId, VecDeque<TurnId>>>,
    project_roots: Mutex<HashMap<SessionId, String>>,
    delivered_hook_turns: Mutex<HashMap<SessionId, String>>,
    delivered_progress_events: Mutex<HashMap<SessionId, String>>,
    polling_sessions: Mutex<HashSet<SessionId>>,
    poller_tasks: Mutex<HashMap<SessionId, JoinHandle<()>>>,
}

impl Default for LocalRuntimeState {
    fn default() -> Self {
        Self {
            event_sink: OnceCell::new(),
            pending_turns: Mutex::new(HashMap::new()),
            project_roots: Mutex::new(HashMap::new()),
            delivered_hook_turns: Mutex::new(HashMap::new()),
            delivered_progress_events: Mutex::new(HashMap::new()),
            polling_sessions: Mutex::new(HashSet::new()),
            poller_tasks: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
pub trait TmuxClient: Send + Sync {
    async fn exec(&self, args: &[&str]) -> anyhow::Result<()>;
    async fn has_session(&self, target: &str) -> anyhow::Result<bool>;
    async fn capture_pane(&self, target: &str) -> anyhow::Result<String>;
    async fn list_sessions(&self) -> anyhow::Result<Vec<String>>;
    async fn kill_session(&self, target: &str) -> anyhow::Result<()>;
}

#[derive(Clone, Copy)]
pub struct SystemTmuxClient;

#[async_trait]
impl TmuxClient for SystemTmuxClient {
    async fn exec(&self, args: &[&str]) -> anyhow::Result<()> {
        let status = Command::new("tmux")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        if status.success() {
            return Ok(());
        }

        Err(anyhow::anyhow!("tmux command failed with status {status}"))
    }

    async fn has_session(&self, target: &str) -> anyhow::Result<bool> {
        let status = Command::new("tmux")
            .args(["has-session", "-t", target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        Ok(status.success())
    }

    async fn capture_pane(&self, target: &str) -> anyhow::Result<String> {
        let output = Command::new("tmux")
            .args(["capture-pane", "-pt", target])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("tmux capture-pane failed with status {}", output.status));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn list_sessions(&self) -> anyhow::Result<Vec<String>> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#S"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    async fn kill_session(&self, target: &str) -> anyhow::Result<()> {
        self.exec(&["kill-session", "-t", target]).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRuntimeConfig {
    pub working_directory: String,
    pub launch_command: String,
    pub hook_events_directory: String,
}

pub struct LocalRuntime<T> {
    tmux: T,
    config: LocalRuntimeConfig,
    state: Arc<LocalRuntimeState>,
}

impl<T> Clone for LocalRuntime<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Self {
            tmux: self.tmux.clone(),
            config: self.config.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<T> LocalRuntime<T>
where
    T: TmuxClient + Clone + 'static,
{
    pub fn new(tmux: T, config: LocalRuntimeConfig) -> Self {
        Self {
            tmux,
            config,
            state: Arc::new(LocalRuntimeState::default()),
        }
    }

    pub fn set_event_sink(&self, sink: Arc<dyn SessionMessageSink>) -> anyhow::Result<()> {
        self.state
            .event_sink
            .set(sink)
            .map_err(|_| anyhow::anyhow!("runtime event sink already configured"))
    }

    pub fn has_event_sink(&self) -> bool {
        self.state.event_sink.get().is_some()
    }

    pub async fn cleanup_orphan_tmux_sessions(
        &self,
        active_session_ids: &[SessionId],
    ) -> anyhow::Result<Vec<String>> {
        let active: std::collections::HashSet<String> = active_session_ids
            .iter()
            .map(|session_id| session_id.0.to_string())
            .collect();
        let mut removed = Vec::new();

        for session_name in self.tmux.list_sessions().await? {
            if uuid::Uuid::parse_str(&session_name).is_err() {
                continue;
            }

            if active.contains(&session_name) {
                continue;
            }

            self.tmux.kill_session(&session_name).await?;
            removed.push(session_name);
        }

        Ok(removed)
    }

    pub async fn current_turn(&self, session_id: SessionId) -> Option<TurnId> {
        self.state
            .pending_turns
            .lock()
            .await
            .get(&session_id)
            .and_then(|turns| turns.back().copied())
    }

    pub async fn recover_active_turn(&self, session_id: SessionId, turn_id: TurnId) {
        let mut pending_turns = self.state.pending_turns.lock().await;
        let turns = pending_turns.entry(session_id).or_default();
        if !turns.iter().any(|existing| *existing == turn_id) {
            turns.push_back(turn_id);
        }
        drop(pending_turns);
        self.start_hook_poller(session_id).await;
    }

    async fn dequeue_turn(&self, session_id: SessionId) -> Option<TurnId> {
        self.state
            .pending_turns
            .lock()
            .await
            .get_mut(&session_id)
            .and_then(|turns| turns.pop_back())
    }

    pub async fn register_project_root(&self, session_id: SessionId, project_root: String) {
        self.state
            .project_roots
            .lock()
            .await
            .insert(session_id, project_root);
    }

    pub async fn project_root(&self, session_id: SessionId) -> Option<String> {
        self.state.project_roots.lock().await.get(&session_id).cloned()
    }

    pub async fn emit_runtime_completed(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        summary: impl Into<String>,
    ) -> anyhow::Result<()> {
        let sink = self
            .state
            .event_sink
            .get()
            .ok_or_else(|| anyhow::anyhow!("runtime event sink not configured"))?;
        sink.send_to_session(
            session_id,
            SessionMsg::RuntimeCompleted {
                turn_id,
                summary: summary.into(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn emit_runtime_failed(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        error: impl Into<String>,
    ) -> anyhow::Result<()> {
        let sink = self
            .state
            .event_sink
            .get()
            .ok_or_else(|| anyhow::anyhow!("runtime event sink not configured"))?;
        sink.send_to_session(
            session_id,
            SessionMsg::RuntimeFailed {
                turn_id,
                error: error.into(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn emit_runtime_progress(
        &self,
        session_id: SessionId,
        text: impl Into<String>,
    ) -> anyhow::Result<()> {
        let sink = self
            .state
            .event_sink
            .get()
            .ok_or_else(|| anyhow::anyhow!("runtime event sink not configured"))?;
        sink.send_to_session(
            session_id,
            SessionMsg::RuntimeProgress { text: text.into() },
        )
        .await?;
        Ok(())
    }

    pub async fn emit_current_turn_completed(
        &self,
        session_id: SessionId,
        summary: impl Into<String>,
    ) -> anyhow::Result<()> {
        let turn_id = self
            .dequeue_turn(session_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("no pending turn registered for session"))?;
        self.emit_runtime_completed(session_id, turn_id, summary).await
    }

    pub async fn emit_current_turn_failed(
        &self,
        session_id: SessionId,
        error: impl Into<String>,
    ) -> anyhow::Result<()> {
        let turn_id = self
            .dequeue_turn(session_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("no pending turn registered for session"))?;
        self.emit_runtime_failed(session_id, turn_id, error).await
    }

    pub fn hook_event_file_path(&self, session_id: SessionId) -> String {
        format!(
            "{}/{}.events.jsonl",
            self.config.hook_events_directory, session_id.0
        )
    }

    async fn append_hook_event(&self, session_id: SessionId, event: &HookRelayEvent) -> anyhow::Result<()> {
        let payload = format!("{}\n", serde_json::to_string(event)?);
        let file_path = self.hook_event_file_path(session_id);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file_path)
            ?;
        use std::io::Write;
        file.write_all(payload.as_bytes())?;
        Ok(())
    }

    async fn recover_terminal_event(
        &self,
        session_id: SessionId,
        last_delivered_turn_id: Option<&str>,
        events: &[HookRelayEvent],
    ) -> anyhow::Result<Option<HookRelayEvent>> {
        let pane = self.tmux.capture_pane(&session_id.0.to_string()).await?;
        let Some(idle_prompt_input) = get_idle_prompt_input(&pane) else {
            return Ok(None);
        };

        if !idle_prompt_input.is_empty() {
            return Ok(Some(HookRelayEvent {
                session_id: session_id.0.to_string(),
                turn_id: format!("{}:{}", session_id.0, SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()),
                event: HookRelayEventKind::Notification,
                text: format!(
                    "Claude input was not executed and is still sitting at the prompt: {idle_prompt_input}"
                ),
                created_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .to_string(),
            }));
        }

        let project_root = self
            .project_root(session_id)
            .await
            .unwrap_or_else(|| self.config.working_directory.clone());
        let Some(claude_session_id) =
            extract_claude_session_id(session_id, last_delivered_turn_id, events)
        else {
            return Ok(None);
        };
        let transcript_path = build_project_session_log_path(&project_root, &claude_session_id);
        let last_assistant_text = read_last_assistant_text_from_transcript(&transcript_path).await?;
        let last_terminal_text = events
            .iter()
            .filter(|event| is_assistant_terminal_event(event))
            .next_back()
            .map(|event| event.text.as_str())
            .unwrap_or_default();

        if last_assistant_text.is_empty() || last_assistant_text == last_terminal_text {
            return Ok(None);
        }

        Ok(Some(synthetic_stop_event(
            session_id,
            &claude_session_id,
            last_assistant_text,
        )))
    }

    pub async fn poll_hook_events_once(&self, session_id: SessionId) -> anyhow::Result<()> {
        let hook_event_file_path = self.hook_event_file_path(session_id);
        let mut events = read_hook_events(&hook_event_file_path).await?;
        let last_delivered = self
            .state
            .delivered_hook_turns
            .lock()
            .await
            .get(&session_id)
            .cloned();
        let has_undelivered_assistant_terminal = pick_undelivered_terminal_events(
            last_delivered.as_deref(),
            &events,
        )
        .into_iter()
        .any(|event| is_assistant_terminal_event(&event));

        if self.current_turn(session_id).await.is_some() && !has_undelivered_assistant_terminal {
            if let Some(recovered) = self
                .recover_terminal_event(session_id, last_delivered.as_deref(), &events)
                .await?
            {
                self.append_hook_event(session_id, &recovered).await?;
                events.push(recovered);
            }
        }

        if let Some(progress_event) = pick_latest_progress_event(last_delivered.as_deref(), &events) {
            let progress_key = format!(
                "{}:{}:{}",
                progress_event.turn_id, progress_event.created_at, progress_event.text
            );
            let mut delivered_progress_events = self.state.delivered_progress_events.lock().await;
            if delivered_progress_events.get(&session_id) != Some(&progress_key) {
                self.emit_runtime_progress(session_id, progress_event.text.clone())
                    .await?;
                delivered_progress_events.insert(session_id, progress_key);
            }
        }
        let undelivered = pick_undelivered_terminal_events(last_delivered.as_deref(), &events);

        for event in undelivered {
            match event.event {
                HookRelayEventKind::Stop => {
                    if self.current_turn(session_id).await.is_some() {
                        self.emit_current_turn_completed(session_id, event.text.clone())
                            .await?;
                    }
                }
                HookRelayEventKind::StopFailure => {
                    if self.current_turn(session_id).await.is_some() {
                        self.emit_current_turn_failed(session_id, event.text.clone())
                            .await?;
                    }
                }
                HookRelayEventKind::Notification => {
                    if self.current_turn(session_id).await.is_some() {
                        self.emit_current_turn_completed(session_id, event.text.clone())
                            .await?;
                    }
                }
                HookRelayEventKind::PreToolUse | HookRelayEventKind::PostToolUse => continue,
            }

            self.state
                .delivered_hook_turns
                .lock()
                .await
                .insert(session_id, event.turn_id);
            self.state
                .delivered_progress_events
                .lock()
                .await
                .remove(&session_id);
        }

        Ok(())
    }

    pub async fn start_hook_poller(&self, session_id: SessionId) {
        let mut polling_sessions = self.state.polling_sessions.lock().await;
        if !polling_sessions.insert(session_id) {
            return;
        }
        drop(polling_sessions);

        let runtime = self.clone();
        let task = tokio::spawn(async move {
            loop {
                if !runtime.state.polling_sessions.lock().await.contains(&session_id) {
                    break;
                }
                let _ = runtime.poll_hook_events_once(session_id).await;
                sleep(Duration::from_secs(2)).await;
            }
        });

        self.state.poller_tasks.lock().await.insert(session_id, task);
    }

    pub async fn stop_hook_poller(&self, session_id: SessionId) {
        self.state.polling_sessions.lock().await.remove(&session_id);
        if let Some(task) = self.state.poller_tasks.lock().await.remove(&session_id) {
            task.abort();
        }
    }

    pub async fn clear_runtime_bookkeeping(&self, session_id: SessionId) -> anyhow::Result<()> {
        self.stop_hook_poller(session_id).await;
        self.state.pending_turns.lock().await.remove(&session_id);
        self.state.project_roots.lock().await.remove(&session_id);
        self.state.delivered_hook_turns.lock().await.remove(&session_id);
        self.state.delivered_progress_events.lock().await.remove(&session_id);
        Ok(())
    }
}

#[async_trait]
impl<T> RuntimeEngine for LocalRuntime<T>
where
    T: TmuxClient + Clone + 'static,
{
    async fn handle(
        &self,
        session_id: SessionId,
        message: &SessionMsg,
        next_state: &SessionState,
    ) -> anyhow::Result<()> {
        let target = session_id.0.to_string();

        if let SessionState::Running { active_turn } = next_state {
            let mut pending_turns = self.state.pending_turns.lock().await;
            let turns = pending_turns.entry(session_id).or_default();
            if turns.back().copied() != Some(*active_turn) {
                turns.push_back(*active_turn);
            }
        }

        match message {
            SessionMsg::Recover => {
                self.start_hook_poller(session_id).await;
                if !self.tmux.has_session(&target).await? {
                    let working_directory = self
                        .project_root(session_id)
                        .await
                        .unwrap_or_else(|| self.config.working_directory.clone());
                    self.tmux
                        .exec(&[
                            "new-session",
                            "-d",
                            "-s",
                            &target,
                            "-c",
                            &working_directory,
                        ])
                        .await?;
                    self.tmux
                        .exec(&[
                            "send-keys",
                            "-t",
                            &target,
                            "-l",
                            "--",
                            &format!(
                                "export SLACK_REMOTE_HOOK_EVENT_FILE={}",
                                self.hook_event_file_path(session_id)
                            ),
                        ])
                        .await?;
                    self.tmux.exec(&["send-keys", "-t", &target, "Enter"]).await?;
                    self.tmux
                        .exec(&[
                            "send-keys",
                            "-t",
                            &target,
                            "-l",
                            "--",
                            &format!("export SLACK_REMOTE_SESSION_ID={target}"),
                        ])
                        .await?;
                    self.tmux.exec(&["send-keys", "-t", &target, "Enter"]).await?;
                    self.tmux
                        .exec(&[
                            "send-keys",
                            "-t",
                            &target,
                            "-l",
                            "--",
                            &format!("export SLACK_REMOTE_PROJECT_ROOT={working_directory}"),
                        ])
                        .await?;
                    self.tmux.exec(&["send-keys", "-t", &target, "Enter"]).await?;
                    self.tmux
                        .exec(&["send-keys", "-t", &target, "-l", "--", &self.config.launch_command])
                        .await?;
                    self.tmux.exec(&["send-keys", "-t", &target, "Enter"]).await?;
                }
            }
            SessionMsg::UserCommand(command) => {
                self.tmux.exec(&["send-keys", "-t", &target, "C-u"]).await?;
                self.tmux
                    .exec(&["send-keys", "-t", &target, "-l", "--", &command.text])
                    .await?;
                self.tmux.exec(&["send-keys", "-t", &target, "Enter"]).await?;
            }
            SessionMsg::SendKey { key } => {
                self.tmux.exec(&["send-keys", "-t", &target, key]).await?;
            }
            SessionMsg::Interrupt => {
                self.tmux.exec(&["send-keys", "-t", &target, "C-c"]).await?;
            }
            SessionMsg::Terminate => {
                self.tmux.kill_session(&target).await?;
            }
            SessionMsg::ApprovalGranted
            | SessionMsg::ApprovalRejected
            | SessionMsg::RuntimeProgress { .. }
            | SessionMsg::RuntimeCompleted { .. }
            | SessionMsg::RuntimeFailed { .. } => {}
        }

        Ok(())
    }
}

#[async_trait]
impl<T> SessionRuntimeConfigurator for LocalRuntime<T>
where
    T: TmuxClient + Clone + 'static,
{
    async fn register_project_root(&self, session_id: SessionId, project_root: &str) -> anyhow::Result<()> {
        self.register_project_root(session_id, project_root.to_string()).await;
        Ok(())
    }
}

#[async_trait]
impl<T> SessionRuntimeLiveness for LocalRuntime<T>
where
    T: TmuxClient + Clone + 'static,
{
    async fn is_session_alive(&self, session_id: SessionId) -> anyhow::Result<bool> {
        self.tmux.has_session(&session_id.0.to_string()).await
    }
}

#[async_trait]
impl<T> SessionRuntimeCleanup for LocalRuntime<T>
where
    T: TmuxClient + Clone + 'static,
{
    async fn clear_runtime_bookkeeping(&self, session_id: SessionId) -> anyhow::Result<()> {
        LocalRuntime::clear_runtime_bookkeeping(self, session_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use core_model::{TurnId, UserCommand};
    use core_service::SessionMessageSink;
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Default, Clone)]
    struct RecordingTmux {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        has_session: Arc<Mutex<bool>>,
        sessions: Arc<Mutex<Vec<String>>>,
        pane: Arc<Mutex<String>>,
    }

    #[async_trait]
    impl TmuxClient for RecordingTmux {
        async fn exec(&self, args: &[&str]) -> anyhow::Result<()> {
            self.calls
                .lock()
                .await
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            Ok(())
        }

        async fn has_session(&self, _target: &str) -> anyhow::Result<bool> {
            Ok(*self.has_session.lock().await)
        }

        async fn capture_pane(&self, _target: &str) -> anyhow::Result<String> {
            Ok(self.pane.lock().await.clone())
        }

        async fn list_sessions(&self) -> anyhow::Result<Vec<String>> {
            Ok(self.sessions.lock().await.clone())
        }

        async fn kill_session(&self, target: &str) -> anyhow::Result<()> {
            self.calls
                .lock()
                .await
                .push(vec!["kill-session".to_string(), "-t".to_string(), target.to_string()]);
            Ok(())
        }
    }

    fn test_runtime(tmux: RecordingTmux) -> LocalRuntime<RecordingTmux> {
        LocalRuntime::new(
            tmux,
            LocalRuntimeConfig {
                working_directory: "/tmp/project".to_string(),
                launch_command: "claude --dangerously-skip-permissions".to_string(),
                hook_events_directory: "/tmp/rcc-hooks".to_string(),
            },
        )
    }

    #[derive(Default, Clone)]
    struct RecordingSink {
        calls: Arc<Mutex<Vec<(SessionId, SessionMsg)>>>,
    }

    #[async_trait]
    impl SessionMessageSink for RecordingSink {
        async fn send_to_session(
            &self,
            session_id: SessionId,
            message: SessionMsg,
        ) -> anyhow::Result<core_model::SessionState> {
            self.calls.lock().await.push((session_id, message));
            Ok(core_model::SessionState::Idle)
        }
    }

    #[tokio::test]
    async fn user_command_is_sent_as_tmux_reply_sequence() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux.clone());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();

        runtime
            .handle(
                session_id,
                &SessionMsg::UserCommand(UserCommand {
                    text: "continue with fix".to_string(),
                }),
                &SessionState::Running { active_turn: turn_id },
            )
            .await
            .expect("handle user command");

        let calls = tmux.calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![
                vec!["send-keys".to_string(), "-t".to_string(), session_id.0.to_string(), "C-u".to_string()],
                vec![
                    "send-keys".to_string(),
                    "-t".to_string(),
                    session_id.0.to_string(),
                    "-l".to_string(),
                    "--".to_string(),
                    "continue with fix".to_string(),
                ],
                vec!["send-keys".to_string(), "-t".to_string(), session_id.0.to_string(), "Enter".to_string()],
            ]
        );
    }

    #[tokio::test]
    async fn interrupt_sends_control_c_to_tmux() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux.clone());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();

        runtime
            .handle(
                session_id,
                &SessionMsg::Interrupt,
                &SessionState::Cancelling { active_turn: turn_id },
            )
            .await
            .expect("handle interrupt");

        let calls = tmux.calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![vec![
                "send-keys".to_string(),
                "-t".to_string(),
                session_id.0.to_string(),
                "C-c".to_string(),
            ]]
        );
    }

    #[tokio::test]
    async fn send_key_forwards_raw_tmux_key() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux.clone());
        let session_id = SessionId::new();

        runtime
            .handle(
                session_id,
                &SessionMsg::SendKey {
                    key: "Escape".to_string(),
                },
                &SessionState::Idle,
            )
            .await
            .expect("handle send key");

        let calls = tmux.calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![vec![
                "send-keys".to_string(),
                "-t".to_string(),
                session_id.0.to_string(),
                "Escape".to_string(),
            ]]
        );
    }

    #[tokio::test]
    async fn terminate_kills_tmux_session() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux.clone());
        let session_id = SessionId::new();

        runtime
            .handle(session_id, &SessionMsg::Terminate, &SessionState::Completed)
            .await
            .expect("handle terminate");

        let calls = tmux.calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![vec![
                "kill-session".to_string(),
                "-t".to_string(),
                session_id.0.to_string(),
            ]]
        );
    }

    #[tokio::test]
    async fn recover_starts_tmux_session_and_launches_claude_when_missing() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux.clone());
        let session_id = SessionId::new();

        runtime
            .handle(session_id, &SessionMsg::Recover, &SessionState::Idle)
            .await
            .expect("handle recover");

        let calls = tmux.calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![
                vec![
                    "new-session".to_string(),
                    "-d".to_string(),
                    "-s".to_string(),
                    session_id.0.to_string(),
                    "-c".to_string(),
                    "/tmp/project".to_string(),
                ],
                vec![
                    "send-keys".to_string(),
                    "-t".to_string(),
                    session_id.0.to_string(),
                    "-l".to_string(),
                    "--".to_string(),
                    format!(
                        "export SLACK_REMOTE_HOOK_EVENT_FILE=/tmp/rcc-hooks/{}.events.jsonl",
                        session_id.0
                    ),
                ],
                vec!["send-keys".to_string(), "-t".to_string(), session_id.0.to_string(), "Enter".to_string()],
                vec![
                    "send-keys".to_string(),
                    "-t".to_string(),
                    session_id.0.to_string(),
                    "-l".to_string(),
                    "--".to_string(),
                    format!("export SLACK_REMOTE_SESSION_ID={}", session_id.0),
                ],
                vec!["send-keys".to_string(), "-t".to_string(), session_id.0.to_string(), "Enter".to_string()],
                vec![
                    "send-keys".to_string(),
                    "-t".to_string(),
                    session_id.0.to_string(),
                    "-l".to_string(),
                    "--".to_string(),
                    "export SLACK_REMOTE_PROJECT_ROOT=/tmp/project".to_string(),
                ],
                vec!["send-keys".to_string(), "-t".to_string(), session_id.0.to_string(), "Enter".to_string()],
                vec![
                    "send-keys".to_string(),
                    "-t".to_string(),
                    session_id.0.to_string(),
                    "-l".to_string(),
                    "--".to_string(),
                    "claude --dangerously-skip-permissions".to_string(),
                ],
                vec!["send-keys".to_string(), "-t".to_string(), session_id.0.to_string(), "Enter".to_string()],
            ]
        );
    }

    #[tokio::test]
    async fn recover_skips_launch_when_tmux_session_already_exists() {
        let tmux = RecordingTmux::default();
        *tmux.has_session.lock().await = true;
        let runtime = test_runtime(tmux.clone());

        runtime
            .handle(SessionId::new(), &SessionMsg::Recover, &SessionState::Idle)
            .await
            .expect("handle recover");

        assert!(tmux.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cleanup_orphan_tmux_sessions_kills_only_uuid_sessions_not_in_repository() {
        let tmux = RecordingTmux::default();
        let active = SessionId::new();
        let orphan = SessionId::new();
        *tmux.sessions.lock().await = vec![
            active.0.to_string(),
            orphan.0.to_string(),
            "slack-1775904089765".to_string(),
        ];
        let runtime = test_runtime(tmux.clone());

        let removed = runtime
            .cleanup_orphan_tmux_sessions(&[active])
            .await
            .expect("cleanup orphan tmux sessions");

        assert_eq!(removed, vec![orphan.0.to_string()]);
        assert_eq!(
            tmux.calls.lock().await.as_slice(),
            &[vec![
                "kill-session".to_string(),
                "-t".to_string(),
                orphan.0.to_string(),
            ]]
        );
    }

    #[tokio::test]
    async fn clear_runtime_bookkeeping_removes_pending_turns_and_progress_tracking() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux);
        let session_id = SessionId::new();
        let turn_id = TurnId::new();

        runtime.recover_active_turn(session_id, turn_id).await;
        runtime
            .state
            .delivered_hook_turns
            .lock()
            .await
            .insert(session_id, "turn-1".to_string());
        runtime
            .state
            .delivered_progress_events
            .lock()
            .await
            .insert(session_id, "progress-1".to_string());

        runtime
            .clear_runtime_bookkeeping(session_id)
            .await
            .expect("clear runtime bookkeeping");

        assert_eq!(runtime.current_turn(session_id).await, None);
        assert!(!runtime.state.delivered_hook_turns.lock().await.contains_key(&session_id));
        assert!(!runtime.state.delivered_progress_events.lock().await.contains_key(&session_id));
    }

    #[tokio::test]
    async fn emit_runtime_completed_forwards_event_to_registered_sink() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux);
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();

        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .emit_runtime_completed(session_id, turn_id, "done")
            .await
            .expect("emit runtime completed");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeCompleted {
                    turn_id,
                    summary: "done".to_string(),
                },
            )]
        );
    }

    #[tokio::test]
    async fn emit_runtime_failed_forwards_event_to_registered_sink() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux);
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();

        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .emit_runtime_failed(session_id, turn_id, "boom")
            .await
            .expect("emit runtime failed");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeFailed {
                    turn_id,
                    error: "boom".to_string(),
                },
            )]
        );
    }

    #[tokio::test]
    async fn emit_runtime_progress_forwards_event_to_registered_sink() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux);
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();

        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .emit_runtime_progress(session_id, "done")
            .await
            .expect("emit runtime progress");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeProgress {
                    text: "done".to_string(),
                },
            )]
        );
    }

    #[tokio::test]
    async fn user_command_registers_active_turn_from_next_state() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux);
        let session_id = SessionId::new();
        let turn_id = TurnId::new();

        runtime
            .handle(
                session_id,
                &SessionMsg::UserCommand(UserCommand {
                    text: "continue".to_string(),
                }),
                &SessionState::Running { active_turn: turn_id },
            )
            .await
            .expect("handle user command");

        assert_eq!(runtime.current_turn(session_id).await, Some(turn_id));
    }

    #[tokio::test]
    async fn emit_current_turn_completed_uses_registered_turn() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux);
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();

        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .handle(
                session_id,
                &SessionMsg::UserCommand(UserCommand {
                    text: "continue".to_string(),
                }),
                &SessionState::Running { active_turn: turn_id },
            )
            .await
            .expect("handle user command");
        runtime
            .emit_current_turn_completed(session_id, "done")
            .await
            .expect("emit current turn completed");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeCompleted {
                    turn_id,
                    summary: "done".to_string(),
                },
            )]
        );
        assert_eq!(runtime.current_turn(session_id).await, None);
    }

    #[tokio::test]
    async fn emit_current_turn_completed_uses_latest_pending_turn() {
        let tmux = RecordingTmux::default();
        let runtime = test_runtime(tmux);
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let first_turn = TurnId::new();
        let second_turn = TurnId::new();

        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .handle(
                session_id,
                &SessionMsg::UserCommand(UserCommand {
                    text: "first".to_string(),
                }),
                &SessionState::Running {
                    active_turn: first_turn,
                },
            )
            .await
            .expect("handle first user command");
        runtime
            .handle(
                session_id,
                &SessionMsg::UserCommand(UserCommand {
                    text: "second".to_string(),
                }),
                &SessionState::Running {
                    active_turn: second_turn,
                },
            )
            .await
            .expect("handle second user command");

        runtime
            .emit_current_turn_completed(session_id, "done")
            .await
            .expect("emit current turn completed");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeCompleted {
                    turn_id: second_turn,
                    summary: "done".to_string(),
                },
            )]
        );
        assert_eq!(runtime.current_turn(session_id).await, Some(first_turn));
    }

    #[tokio::test]
    async fn poll_hook_events_once_emits_runtime_completed_from_stop_event() {
        let temp_dir = tempdir().expect("create temp dir");
        let tmux = RecordingTmux::default();
        let runtime = LocalRuntime::new(
            tmux,
            LocalRuntimeConfig {
                working_directory: "/tmp/project".to_string(),
                launch_command: "claude --dangerously-skip-permissions".to_string(),
                hook_events_directory: temp_dir.path().display().to_string(),
            },
        );
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .handle(
                session_id,
                &SessionMsg::UserCommand(UserCommand {
                    text: "continue".to_string(),
                }),
                &SessionState::Running { active_turn: turn_id },
            )
            .await
            .expect("handle user command");
        fs::write(
            runtime.hook_event_file_path(session_id),
            "{\"sessionId\":\"s\",\"turnId\":\"hook-1\",\"event\":\"Stop\",\"text\":\"done\",\"createdAt\":\"2026-04-11T00:00:00Z\"}\n",
        )
        .await
        .expect("write hook events");

        runtime
            .poll_hook_events_once(session_id)
            .await
            .expect("poll hook events");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeCompleted {
                    turn_id,
                    summary: "done".to_string(),
                },
            )]
        );
    }

    #[tokio::test]
    async fn poll_hook_events_once_ignores_stop_without_pending_turn() {
        let temp_dir = tempdir().expect("create temp dir");
        let tmux = RecordingTmux::default();
        let runtime = LocalRuntime::new(
            tmux,
            LocalRuntimeConfig {
                working_directory: "/tmp/project".to_string(),
                launch_command: "claude --dangerously-skip-permissions".to_string(),
                hook_events_directory: temp_dir.path().display().to_string(),
            },
        );
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        fs::write(
            runtime.hook_event_file_path(session_id),
            "{\"sessionId\":\"s\",\"turnId\":\"hook-1\",\"event\":\"Stop\",\"text\":\"ready\",\"createdAt\":\"2026-04-11T00:00:00Z\"}\n",
        )
        .await
        .expect("write hook events");

        runtime
            .poll_hook_events_once(session_id)
            .await
            .expect("poll hook events");

        assert!(sink.calls.lock().await.is_empty());
        assert_eq!(runtime.current_turn(session_id).await, None);
    }

    #[tokio::test]
    async fn poll_hook_events_once_emits_runtime_completed_for_recovered_running_turn() {
        let temp_dir = tempdir().expect("create temp dir");
        let tmux = RecordingTmux::default();
        let runtime = LocalRuntime::new(
            tmux,
            LocalRuntimeConfig {
                working_directory: "/tmp/project".to_string(),
                launch_command: "claude --dangerously-skip-permissions".to_string(),
                hook_events_directory: temp_dir.path().display().to_string(),
            },
        );
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .recover_active_turn(session_id, turn_id)
            .await;
        fs::write(
            runtime.hook_event_file_path(session_id),
            "{\"sessionId\":\"s\",\"turnId\":\"hook-1\",\"event\":\"Stop\",\"text\":\"done\",\"createdAt\":\"2026-04-11T00:00:00Z\"}\n",
        )
        .await
        .expect("write hook events");

        runtime
            .poll_hook_events_once(session_id)
            .await
            .expect("poll hook events");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeCompleted {
                    turn_id,
                    summary: "done".to_string(),
                },
            )]
        );
    }

    #[tokio::test]
    async fn poll_hook_events_once_emits_latest_progress_event() {
        let temp_dir = tempdir().expect("create temp dir");
        let tmux = RecordingTmux::default();
        let runtime = LocalRuntime::new(
            tmux,
            LocalRuntimeConfig {
                working_directory: "/tmp/project".to_string(),
                launch_command: "claude --dangerously-skip-permissions".to_string(),
                hook_events_directory: temp_dir.path().display().to_string(),
            },
        );
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        fs::write(
            runtime.hook_event_file_path(session_id),
            concat!(
                "{\"sessionId\":\"s\",\"turnId\":\"hook-1\",\"event\":\"PreToolUse\",\"text\":\"Read\",\"createdAt\":\"2026-04-11T00:00:00Z\"}\n",
                "{\"sessionId\":\"s\",\"turnId\":\"hook-1\",\"event\":\"PostToolUse\",\"text\":\"done\",\"createdAt\":\"2026-04-11T00:00:01Z\"}\n"
            ),
        )
        .await
        .expect("write hook events");

        runtime
            .poll_hook_events_once(session_id)
            .await
            .expect("poll hook events");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeProgress {
                    text: "done".to_string(),
                },
            )]
        );
    }

    #[tokio::test]
    async fn poll_hook_events_once_does_not_redeliver_same_terminal_event() {
        let temp_dir = tempdir().expect("create temp dir");
        let tmux = RecordingTmux::default();
        let runtime = LocalRuntime::new(
            tmux,
            LocalRuntimeConfig {
                working_directory: "/tmp/project".to_string(),
                launch_command: "claude --dangerously-skip-permissions".to_string(),
                hook_events_directory: temp_dir.path().display().to_string(),
            },
        );
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .handle(
                session_id,
                &SessionMsg::UserCommand(UserCommand {
                    text: "continue".to_string(),
                }),
                &SessionState::Running { active_turn: turn_id },
            )
            .await
            .expect("handle user command");
        fs::write(
            runtime.hook_event_file_path(session_id),
            "{\"sessionId\":\"s\",\"turnId\":\"hook-1\",\"event\":\"StopFailure\",\"text\":\"boom\",\"createdAt\":\"2026-04-11T00:00:00Z\"}\n",
        )
        .await
        .expect("write hook events");

        runtime
            .poll_hook_events_once(session_id)
            .await
            .expect("first poll");
        runtime
            .poll_hook_events_once(session_id)
            .await
            .expect("second poll");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeFailed {
                    turn_id,
                    error: "boom".to_string(),
                },
            )]
        );
    }

    #[tokio::test]
    async fn poll_hook_events_once_treats_waiting_for_input_notification_as_terminal() {
        let temp_dir = tempdir().expect("create temp dir");
        let tmux = RecordingTmux::default();
        *tmux.pane.lock().await = "answer\n❯\n".to_string();
        let runtime = LocalRuntime::new(
            tmux,
            LocalRuntimeConfig {
                working_directory: "/tmp/project".to_string(),
                launch_command: "claude --dangerously-skip-permissions".to_string(),
                hook_events_directory: temp_dir.path().display().to_string(),
            },
        );
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime
            .recover_active_turn(session_id, turn_id)
            .await;
        fs::write(
            runtime.hook_event_file_path(session_id),
            "{\"sessionId\":\"s\",\"turnId\":\"hook-1\",\"event\":\"Notification\",\"text\":\"Claude is waiting for your input\",\"createdAt\":\"2026-04-11T00:00:00Z\"}\n",
        )
        .await
        .expect("write hook events");

        runtime
            .poll_hook_events_once(session_id)
            .await
            .expect("poll hook events");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeCompleted {
                    turn_id,
                    summary: "Claude is waiting for your input".to_string(),
                },
            )]
        );
    }

    #[tokio::test]
    async fn poll_hook_events_once_recovers_missing_stop_from_transcript_when_idle() {
        let temp_dir = tempdir().expect("create temp dir");
        let home_dir = temp_dir.path().join("home");
        let project_root = "/tmp/project";
        let claude_session_id = "claude-session-1";
        let original_home = env::var("HOME").ok();
        unsafe { env::set_var("HOME", &home_dir); }
        let transcript_path = build_project_session_log_path(project_root, claude_session_id);
        fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
            .await
            .expect("create transcript parent");
        fs::write(
            &transcript_path,
            concat!(
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Older answer\"}],\"stop_reason\":\"end_turn\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Recovered final answer\"}],\"stop_reason\":\"end_turn\"}}\n"
            ),
        )
        .await
        .expect("write transcript");

        let tmux = RecordingTmux::default();
        *tmux.pane.lock().await = "Recovered final answer\n\n❯\n".to_string();
        let runtime = LocalRuntime::new(
            tmux,
            LocalRuntimeConfig {
                working_directory: project_root.to_string(),
                launch_command: "claude --dangerously-skip-permissions".to_string(),
                hook_events_directory: temp_dir.path().join("hooks").display().to_string(),
            },
        );
        let sink = Arc::new(RecordingSink::default());
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        runtime
            .set_event_sink(sink.clone())
            .expect("register event sink");
        runtime.register_project_root(session_id, project_root.to_string()).await;
        runtime.recover_active_turn(session_id, turn_id).await;
        runtime
            .state
            .delivered_hook_turns
            .lock()
            .await
            .insert(session_id, format!("{claude_session_id}:old-stop"));
        fs::create_dir_all(temp_dir.path().join("hooks"))
            .await
            .expect("create hooks dir");
        fs::write(
            runtime.hook_event_file_path(session_id),
            format!(
                "{{\"sessionId\":\"s\",\"turnId\":\"{claude_session_id}:old-stop\",\"event\":\"Stop\",\"text\":\"Older answer\",\"createdAt\":\"2026-04-11T00:00:00Z\"}}\n{{\"sessionId\":\"s\",\"turnId\":\"{claude_session_id}:notification-2\",\"event\":\"Notification\",\"text\":\"Claude is waiting for your input\",\"createdAt\":\"2026-04-11T00:00:10Z\"}}\n"
            ),
        )
        .await
        .expect("write hook events");

        let result = runtime.poll_hook_events_once(session_id).await;

        if let Some(value) = original_home {
            unsafe { env::set_var("HOME", value); }
        }

        result.expect("poll hook events");

        assert_eq!(
            sink.calls.lock().await.as_slice(),
            &[(
                session_id,
                SessionMsg::RuntimeCompleted {
                    turn_id,
                    summary: "Recovered final answer".to_string(),
                },
            )]
        );
    }

    #[test]
    fn hook_event_file_path_is_scoped_by_session_id() {
        let runtime = test_runtime(RecordingTmux::default());
        let session_id = SessionId::new();

        assert_eq!(
            runtime.hook_event_file_path(session_id),
            format!("/tmp/rcc-hooks/{}.events.jsonl", session_id.0)
        );
    }

    #[tokio::test]
    async fn read_hook_events_ignores_missing_file() {
        let temp_dir = tempdir().expect("create temp dir");
        let missing = temp_dir.path().join("missing.events.jsonl");

        let events = read_hook_events(missing.to_str().expect("path utf8"))
            .await
            .expect("read missing hook events");

        assert!(events.is_empty());
    }

    #[test]
    fn pick_undelivered_terminal_events_skips_previously_delivered_turns() {
        let events = vec![
            HookRelayEvent {
                session_id: "session-1".to_string(),
                turn_id: "turn-1".to_string(),
                event: HookRelayEventKind::Stop,
                text: "done 1".to_string(),
                created_at: "2026-04-11T00:00:00Z".to_string(),
            },
            HookRelayEvent {
                session_id: "session-1".to_string(),
                turn_id: "turn-2".to_string(),
                event: HookRelayEventKind::PostToolUse,
                text: "ignored".to_string(),
                created_at: "2026-04-11T00:00:01Z".to_string(),
            },
            HookRelayEvent {
                session_id: "session-1".to_string(),
                turn_id: "turn-3".to_string(),
                event: HookRelayEventKind::StopFailure,
                text: "boom".to_string(),
                created_at: "2026-04-11T00:00:02Z".to_string(),
            },
        ];

        let undelivered = pick_undelivered_terminal_events(Some("turn-1"), &events);

        assert_eq!(
            undelivered,
            vec![HookRelayEvent {
                session_id: "session-1".to_string(),
                turn_id: "turn-3".to_string(),
                event: HookRelayEventKind::StopFailure,
                text: "boom".to_string(),
                created_at: "2026-04-11T00:00:02Z".to_string(),
            }]
        );
    }

    #[test]
    fn pick_latest_progress_event_returns_latest_since_last_terminal() {
        let events = vec![
            HookRelayEvent {
                session_id: "session-1".to_string(),
                turn_id: "turn-1".to_string(),
                event: HookRelayEventKind::Stop,
                text: "done".to_string(),
                created_at: "2026-04-11T00:00:00Z".to_string(),
            },
            HookRelayEvent {
                session_id: "session-1".to_string(),
                turn_id: "turn-2".to_string(),
                event: HookRelayEventKind::PreToolUse,
                text: "Read".to_string(),
                created_at: "2026-04-11T00:00:01Z".to_string(),
            },
            HookRelayEvent {
                session_id: "session-1".to_string(),
                turn_id: "turn-2".to_string(),
                event: HookRelayEventKind::PostToolUse,
                text: "done".to_string(),
                created_at: "2026-04-11T00:00:02Z".to_string(),
            },
        ];

        let progress = pick_latest_progress_event(Some("turn-1"), &events);

        assert_eq!(
            progress,
            Some(HookRelayEvent {
                session_id: "session-1".to_string(),
                turn_id: "turn-2".to_string(),
                event: HookRelayEventKind::PostToolUse,
                text: "done".to_string(),
                created_at: "2026-04-11T00:00:02Z".to_string(),
            })
        );
    }
}
