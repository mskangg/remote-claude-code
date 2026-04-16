use std::{collections::HashMap, sync::{Arc, OnceLock}};

use async_trait::async_trait;
use core_model::{SessionId, SessionMsg, SessionState, TurnId};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::warn;

#[async_trait]
pub trait SessionRepository: Send + Sync {
    async fn load_state(&self, session_id: SessionId) -> anyhow::Result<Option<SessionState>>;
    async fn save_state(&self, session_id: SessionId, state: &SessionState) -> anyhow::Result<()>;
}

#[async_trait]
impl<T> SessionRepository for Arc<T>
where
    T: SessionRepository + Send + Sync,
{
    async fn load_state(&self, session_id: SessionId) -> anyhow::Result<Option<SessionState>> {
        (**self).load_state(session_id).await
    }

    async fn save_state(&self, session_id: SessionId, state: &SessionState) -> anyhow::Result<()> {
        (**self).save_state(session_id, state).await
    }
}

#[async_trait]
pub trait RuntimeEngine: Send + Sync {
    async fn handle(
        &self,
        session_id: SessionId,
        message: &SessionMsg,
        next_state: &SessionState,
    ) -> anyhow::Result<()>;
}

#[async_trait]
pub trait SessionMessageSink: Send + Sync {
    async fn send_to_session(
        &self,
        session_id: SessionId,
        message: SessionMsg,
    ) -> anyhow::Result<SessionState>;
}

#[async_trait]
pub trait SessionRuntimeConfigurator: Send + Sync {
    async fn register_project_root(
        &self,
        session_id: SessionId,
        project_root: &str,
    ) -> anyhow::Result<()>;
}

#[async_trait]
pub trait SessionRuntimeLiveness: Send + Sync {
    async fn is_session_alive(&self, session_id: SessionId) -> anyhow::Result<bool>;
}

#[async_trait]
pub trait SessionRuntimeCleanup: Send + Sync {
    async fn clear_runtime_bookkeeping(&self, session_id: SessionId) -> anyhow::Result<()>;
}

#[async_trait]
pub trait SessionStateObserver: Send + Sync {
    async fn on_state_changed(
        &self,
        session_id: SessionId,
        message: &SessionMsg,
        next_state: &SessionState,
    ) -> anyhow::Result<()>;
}

pub struct NoopSessionStateObserver;

#[async_trait]
impl SessionStateObserver for NoopSessionStateObserver {
    async fn on_state_changed(
        &self,
        _session_id: SessionId,
        _message: &SessionMsg,
        _next_state: &SessionState,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl<T> RuntimeEngine for Arc<T>
where
    T: RuntimeEngine + Send + Sync,
{
    async fn handle(
        &self,
        session_id: SessionId,
        message: &SessionMsg,
        next_state: &SessionState,
    ) -> anyhow::Result<()> {
        (**self).handle(session_id, message, next_state).await
    }
}

#[async_trait]
impl<T> SessionMessageSink for Arc<T>
where
    T: SessionMessageSink + Send + Sync,
{
    async fn send_to_session(
        &self,
        session_id: SessionId,
        message: SessionMsg,
    ) -> anyhow::Result<SessionState> {
        (**self).send_to_session(session_id, message).await
    }
}

#[async_trait]
impl<T> SessionRuntimeConfigurator for Arc<T>
where
    T: SessionRuntimeConfigurator + Send + Sync,
{
    async fn register_project_root(
        &self,
        session_id: SessionId,
        project_root: &str,
    ) -> anyhow::Result<()> {
        (**self).register_project_root(session_id, project_root).await
    }
}

#[async_trait]
impl<T> SessionRuntimeLiveness for Arc<T>
where
    T: SessionRuntimeLiveness + Send + Sync,
{
    async fn is_session_alive(&self, session_id: SessionId) -> anyhow::Result<bool> {
        (**self).is_session_alive(session_id).await
    }
}

#[async_trait]
impl<T> SessionRuntimeCleanup for Arc<T>
where
    T: SessionRuntimeCleanup + Send + Sync,
{
    async fn clear_runtime_bookkeeping(&self, session_id: SessionId) -> anyhow::Result<()> {
        (**self).clear_runtime_bookkeeping(session_id).await
    }
}

#[async_trait]
impl<T> SessionStateObserver for Arc<T>
where
    T: SessionStateObserver + Send + Sync,
{
    async fn on_state_changed(
        &self,
        session_id: SessionId,
        message: &SessionMsg,
        next_state: &SessionState,
    ) -> anyhow::Result<()> {
        (**self).on_state_changed(session_id, message, next_state).await
    }
}

pub struct SessionActor<R, E> {
    repository: R,
    runtime: E,
    observer: Arc<OnceLock<Arc<dyn SessionStateObserver>>>,
}

impl<R, E> SessionActor<R, E>
where
    R: SessionRepository,
    E: RuntimeEngine + SessionRuntimeCleanup,
{
    pub fn new(
        repository: R,
        runtime: E,
        observer: Arc<OnceLock<Arc<dyn SessionStateObserver>>>,
    ) -> Self {
        Self {
            repository,
            runtime,
            observer,
        }
    }

    pub async fn handle_message(&self, session_id: SessionId, message: SessionMsg) -> anyhow::Result<SessionState> {
        let current_state = self
            .repository
            .load_state(session_id)
            .await?
            .unwrap_or(SessionState::Starting);
        let candidate_state = reduce(current_state.clone(), &message);
        let final_state = if should_forward_to_runtime(&current_state, &message, &candidate_state) {
            match self.runtime.handle(session_id, &message, &candidate_state).await {
                Ok(()) => candidate_state,
                Err(error) => reconcile_runtime_failure(&current_state, &message, error),
            }
        } else {
            candidate_state
        };
        persist_final_state(&self.repository, session_id, &final_state).await?;
        if is_terminal_state(&final_state) {
            if let Err(error) = self.runtime.clear_runtime_bookkeeping(session_id).await {
                warn!(session_id = %session_id.0, error = %error, "failed to clear runtime bookkeeping");
            }
        }
        if let Some(observer) = self.observer.get() {
            observer
                .on_state_changed(session_id, &message, &final_state)
                .await?;
        }
        Ok(final_state)
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    session_id: SessionId,
    sender: mpsc::Sender<SessionRequest>,
}

impl SessionHandle {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub async fn send(&self, message: SessionMsg) -> anyhow::Result<SessionState> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(SessionRequest { message, reply_tx })
            .await
            .map_err(|_| anyhow::anyhow!("session actor mailbox closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("session actor dropped response"))?
    }

    #[doc(hidden)]
    pub fn new_for_tests(
        session_id: SessionId,
        sender: mpsc::Sender<SessionRequest>,
    ) -> Self {
        Self { session_id, sender }
    }
}

#[doc(hidden)]
pub struct SessionRequest {
    pub message: SessionMsg,
    pub reply_tx: oneshot::Sender<anyhow::Result<SessionState>>,
}

pub struct SessionRegistry<R, E> {
    repository: Arc<R>,
    runtime: Arc<E>,
    observer: Arc<OnceLock<Arc<dyn SessionStateObserver>>>,
    handles: Mutex<HashMap<SessionId, SessionHandle>>,
    mailbox_capacity: usize,
}

impl<R, E> SessionRegistry<R, E>
where
    R: SessionRepository + 'static,
    E: RuntimeEngine + SessionRuntimeCleanup + 'static,
{
    pub fn new(repository: Arc<R>, runtime: Arc<E>) -> Self {
        Self {
            repository,
            runtime,
            observer: Arc::new(OnceLock::new()),
            handles: Mutex::new(HashMap::new()),
            mailbox_capacity: 32,
        }
    }

    pub fn set_observer(
        &self,
        observer: Arc<dyn SessionStateObserver>,
    ) -> anyhow::Result<()> {
        self.observer
            .set(observer)
            .map_err(|_| anyhow::anyhow!("session observer already configured"))
    }

    pub async fn session(&self, session_id: SessionId) -> SessionHandle {
        let mut handles = self.handles.lock().await;
        if let Some(handle) = handles.get(&session_id) {
            return handle.clone();
        }

        let handle = spawn_session_actor(
            session_id,
            Arc::clone(&self.repository),
            Arc::clone(&self.runtime),
            Arc::clone(&self.observer),
            self.mailbox_capacity,
        );
        handles.insert(session_id, handle.clone());
        handle
    }

    async fn terminal_state_for_session(&self, session_id: SessionId) -> anyhow::Result<Option<SessionState>> {
        let state = self.repository.load_state(session_id).await?;
        Ok(state.filter(is_terminal_state))
    }

    async fn evict_handle_if_terminal(
        &self,
        session_id: SessionId,
        state: &SessionState,
    ) {
        if is_terminal_state(state) {
            self.handles.lock().await.remove(&session_id);
        }
    }

    pub fn runtime(&self) -> &Arc<E> {
        &self.runtime
    }
}

#[async_trait]
impl<R, E> SessionMessageSink for SessionRegistry<R, E>
where
    R: SessionRepository + Send + Sync + 'static,
    E: RuntimeEngine + SessionRuntimeCleanup + Send + Sync + 'static,
{
    async fn send_to_session(
        &self,
        session_id: SessionId,
        message: SessionMsg,
    ) -> anyhow::Result<SessionState> {
        if let Some(state) = self.terminal_state_for_session(session_id).await? {
            return Ok(state);
        }

        let handle = self.session(session_id).await;
        let state = handle.send(message).await?;
        self.evict_handle_if_terminal(session_id, &state).await;
        Ok(state)
    }
}

fn spawn_session_actor<R, E>(
    session_id: SessionId,
    repository: Arc<R>,
    runtime: Arc<E>,
    observer: Arc<OnceLock<Arc<dyn SessionStateObserver>>>,
    mailbox_capacity: usize,
) -> SessionHandle
where
    R: SessionRepository + 'static,
    E: RuntimeEngine + SessionRuntimeCleanup + 'static,
{
    let (sender, mut receiver) = mpsc::channel::<SessionRequest>(mailbox_capacity);

    tokio::spawn(async move {
        let actor = SessionActor::new(repository, runtime, observer);

        while let Some(request) = receiver.recv().await {
            let result = actor.handle_message(session_id, request.message).await;
            let _ = request.reply_tx.send(result);
        }
    });

    SessionHandle { session_id, sender }
}

fn is_terminal_state(state: &SessionState) -> bool {
    matches!(state, SessionState::Completed | SessionState::Failed { .. })
}

fn reconcile_runtime_failure(
    current_state: &SessionState,
    message: &SessionMsg,
    error: anyhow::Error,
) -> SessionState {
    match message {
        SessionMsg::UserCommand(_) | SessionMsg::Recover => SessionState::Failed {
            reason: error.to_string(),
        },
        SessionMsg::Interrupt
        | SessionMsg::SendKey { .. }
        | SessionMsg::Terminate
        | SessionMsg::ApprovalGranted
        | SessionMsg::ApprovalRejected
        | SessionMsg::RuntimeProgress { .. }
        | SessionMsg::RuntimeCompleted { .. }
        | SessionMsg::RuntimeFailed { .. } => current_state.clone(),
    }
}

async fn persist_final_state<R: SessionRepository>(
    repository: &R,
    session_id: SessionId,
    state: &SessionState,
) -> anyhow::Result<()> {
    repository.save_state(session_id, state).await
}

fn should_forward_to_runtime(
    current_state: &SessionState,
    message: &SessionMsg,
    next_state: &SessionState,
) -> bool {
    match message {
        SessionMsg::UserCommand(_) => matches!(next_state, SessionState::Running { .. }),
        SessionMsg::SendKey { .. } => {
            !matches!(current_state, SessionState::Starting) && !is_terminal_state(current_state)
        },
        SessionMsg::ApprovalGranted | SessionMsg::ApprovalRejected => {
            matches!(current_state, SessionState::WaitingForApproval)
        }
        SessionMsg::RuntimeProgress { .. } => false,
        SessionMsg::Interrupt => matches!(next_state, SessionState::Cancelling { .. }),
        SessionMsg::Terminate => !matches!(
            current_state,
            SessionState::Starting | SessionState::Completed
        ),
        SessionMsg::Recover => matches!(current_state, SessionState::Starting),
        SessionMsg::RuntimeCompleted { .. } | SessionMsg::RuntimeFailed { .. } => false,
    }
}

pub fn reduce(current_state: SessionState, message: &SessionMsg) -> SessionState {
    match (current_state, message) {
        (SessionState::Starting, SessionMsg::Recover) => SessionState::Idle,
        (SessionState::Starting, SessionMsg::UserCommand(_)) => {
            SessionState::Running { active_turn: TurnId::new() }
        }
        (SessionState::Idle, SessionMsg::UserCommand(_)) => {
            SessionState::Running { active_turn: TurnId::new() }
        }
        (SessionState::Running { .. }, SessionMsg::UserCommand(_)) => {
            SessionState::Running { active_turn: TurnId::new() }
        }
        (state, SessionMsg::SendKey { .. }) => state,
        (state, SessionMsg::RuntimeProgress { .. }) => state,
        (SessionState::Running { active_turn }, SessionMsg::Interrupt) => {
            SessionState::Cancelling { active_turn }
        }
        (
            SessionState::Starting
            | SessionState::Idle
            | SessionState::Running { .. }
            | SessionState::WaitingForApproval
            | SessionState::Cancelling { .. }
            | SessionState::Failed { .. },
            SessionMsg::Terminate,
        ) => SessionState::Completed,
        (
            SessionState::Running { active_turn },
            SessionMsg::RuntimeCompleted { turn_id, .. },
        ) if active_turn == *turn_id => SessionState::Idle,
        (
            SessionState::Running { active_turn },
            SessionMsg::RuntimeFailed { turn_id, error },
        ) if active_turn == *turn_id => {
            SessionState::Failed { reason: error.clone() }
        }
        (
            SessionState::Cancelling { active_turn },
            SessionMsg::RuntimeCompleted { turn_id, .. },
        ) if active_turn == *turn_id => SessionState::Idle,
        (
            SessionState::Cancelling { active_turn },
            SessionMsg::RuntimeFailed { turn_id, error },
        ) if active_turn == *turn_id => {
            SessionState::Failed { reason: error.clone() }
        }
        (state, _) => state,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use tokio::sync::Mutex;

    use super::*;
    use core_model::UserCommand;

    #[derive(Default)]
    struct TestRepository {
        states: Mutex<HashMap<SessionId, SessionState>>,
    }

    #[async_trait]
    impl SessionRepository for TestRepository {
        async fn load_state(&self, session_id: SessionId) -> anyhow::Result<Option<SessionState>> {
            Ok(self.states.lock().await.get(&session_id).cloned())
        }

        async fn save_state(&self, session_id: SessionId, state: &SessionState) -> anyhow::Result<()> {
            self.states.lock().await.insert(session_id, state.clone());
            Ok(())
        }
    }

    #[derive(Default, Clone)]
    struct TestRuntime {
        calls: Arc<Mutex<Vec<(SessionMsg, SessionState)>>>,
    }

    #[async_trait]
    impl RuntimeEngine for TestRuntime {
        async fn handle(
            &self,
            _session_id: SessionId,
            message: &SessionMsg,
            next_state: &SessionState,
        ) -> anyhow::Result<()> {
            self.calls
                .lock()
                .await
                .push((message.clone(), next_state.clone()));
            Ok(())
        }
    }

    #[async_trait]
    impl SessionRuntimeCleanup for TestRuntime {
        async fn clear_runtime_bookkeeping(&self, _session_id: SessionId) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FailingRuntime {
        calls: Arc<Mutex<Vec<(SessionMsg, SessionState)>>>,
    }

    impl Default for FailingRuntime {
        fn default() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl RuntimeEngine for FailingRuntime {
        async fn handle(
            &self,
            _session_id: SessionId,
            message: &SessionMsg,
            next_state: &SessionState,
        ) -> anyhow::Result<()> {
            self.calls
                .lock()
                .await
                .push((message.clone(), next_state.clone()));
            Err(anyhow::anyhow!("runtime dispatch failed"))
        }
    }

    #[async_trait]
    impl SessionRuntimeCleanup for FailingRuntime {
        async fn clear_runtime_bookkeeping(&self, _session_id: SessionId) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[derive(Default, Clone)]
    struct RecordingObserver {
        calls: Arc<Mutex<Vec<(SessionMsg, SessionState)>>>,
    }

    #[async_trait]
    impl SessionStateObserver for RecordingObserver {
        async fn on_state_changed(
            &self,
            _session_id: SessionId,
            message: &SessionMsg,
            next_state: &SessionState,
        ) -> anyhow::Result<()> {
            self.calls
                .lock()
                .await
                .push((message.clone(), next_state.clone()));
            Ok(())
        }
    }

    #[derive(Default, Clone)]
    struct RecordingRuntimeCleanup {
        cleaned: Arc<Mutex<Vec<SessionId>>>,
    }

    #[async_trait]
    impl SessionRuntimeCleanup for RecordingRuntimeCleanup {
        async fn clear_runtime_bookkeeping(&self, session_id: SessionId) -> anyhow::Result<()> {
            self.cleaned.lock().await.push(session_id);
            Ok(())
        }
    }

    #[derive(Default, Clone)]
    struct TestRuntimeWithCleanup {
        calls: Arc<Mutex<Vec<(SessionMsg, SessionState)>>>,
        cleanup: RecordingRuntimeCleanup,
    }

    #[async_trait]
    impl RuntimeEngine for TestRuntimeWithCleanup {
        async fn handle(
            &self,
            _session_id: SessionId,
            message: &SessionMsg,
            next_state: &SessionState,
        ) -> anyhow::Result<()> {
            self.calls
                .lock()
                .await
                .push((message.clone(), next_state.clone()));
            Ok(())
        }
    }

    #[async_trait]
    impl SessionRuntimeCleanup for TestRuntimeWithCleanup {
        async fn clear_runtime_bookkeeping(&self, session_id: SessionId) -> anyhow::Result<()> {
            self.cleanup.clear_runtime_bookkeeping(session_id).await
        }
    }

    #[test]
    fn is_terminal_state_matches_completed_and_failed_only() {
        assert!(is_terminal_state(&SessionState::Completed));
        assert!(is_terminal_state(&SessionState::Failed {
            reason: "boom".to_string(),
        }));
        assert!(!is_terminal_state(&SessionState::Idle));
    }

    #[test]
    fn reduce_moves_idle_to_running_for_user_command() {
        let next = reduce(
            SessionState::Idle,
            &SessionMsg::UserCommand(UserCommand {
                text: "analyze failing test".to_string(),
            }),
        );

        assert!(matches!(next, SessionState::Running { .. }));
    }

    #[test]
    fn reduce_ignores_runtime_completion_for_wrong_turn() {
        let active_turn = TurnId::new();
        let next = reduce(
            SessionState::Running { active_turn },
            &SessionMsg::RuntimeCompleted {
                turn_id: TurnId::new(),
                summary: "done".to_string(),
            },
        );

        assert_eq!(next, SessionState::Running { active_turn });
    }

    #[test]
    fn reduce_preserves_active_turn_when_cancelling() {
        let active_turn = TurnId::new();
        let next = reduce(SessionState::Running { active_turn }, &SessionMsg::Interrupt);

        assert_eq!(next, SessionState::Cancelling { active_turn });
    }

    #[test]
    fn reduce_assigns_new_turn_for_user_command_while_running() {
        let active_turn = TurnId::new();
        let next = reduce(
            SessionState::Running { active_turn },
            &SessionMsg::UserCommand(UserCommand {
                text: "follow-up".to_string(),
            }),
        );

        assert!(matches!(next, SessionState::Running { active_turn: next_turn } if next_turn != active_turn));
    }

    #[test]
    fn reduce_leaves_idle_state_unchanged_for_send_key() {
        let next = reduce(
            SessionState::Idle,
            &SessionMsg::SendKey {
                key: "Escape".to_string(),
            },
        );

        assert_eq!(next, SessionState::Idle);
    }

    #[test]
    fn reduce_moves_running_session_to_completed_for_terminate() {
        let next = reduce(
            SessionState::Running {
                active_turn: TurnId::new(),
            },
            &SessionMsg::Terminate,
        );

        assert_eq!(next, SessionState::Completed);
    }

    #[tokio::test]
    async fn actor_loads_current_state_and_skips_runtime_for_completion_events() {
        let repository = TestRepository::default();
        let session_id = SessionId::new();
        repository
            .save_state(session_id, &SessionState::Idle)
            .await
            .expect("save initial state");
        let runtime = TestRuntime::default();
        let actor = SessionActor::new(repository, runtime.clone(), Arc::new(OnceLock::new()));

        let next = actor
            .handle_message(
                session_id,
                SessionMsg::RuntimeCompleted {
                    turn_id: TurnId::new(),
                    summary: "done".to_string(),
                },
            )
            .await
            .expect("handle completion");

        assert_eq!(next, SessionState::Idle);
        assert!(runtime.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn actor_forwards_user_commands_to_runtime() {
        let repository = TestRepository::default();
        let runtime = TestRuntime::default();
        let actor = SessionActor::new(repository, runtime.clone(), Arc::new(OnceLock::new()));
        let session_id = SessionId::new();

        actor
            .handle_message(
                session_id,
                SessionMsg::UserCommand(UserCommand {
                    text: "continue".to_string(),
                }),
            )
            .await
            .expect("handle user command");

        let calls = runtime.calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            calls[0],
            (
                SessionMsg::UserCommand(UserCommand { .. }),
                SessionState::Running { .. }
            )
        ));
    }

    #[tokio::test]
    async fn actor_persists_failed_state_when_user_command_runtime_dispatch_fails() {
        let repository = Arc::new(TestRepository::default());
        let runtime = Arc::new(FailingRuntime::default());
        let observer = Arc::new(RecordingObserver::default());
        let observer_slot: Arc<OnceLock<Arc<dyn SessionStateObserver>>> = Arc::new(OnceLock::new());
        assert!(observer_slot
            .set(observer.clone() as Arc<dyn SessionStateObserver>)
            .is_ok());
        let actor = SessionActor::new(
            Arc::clone(&repository),
            Arc::clone(&runtime),
            observer_slot,
        );
        let session_id = SessionId::new();

        let result = actor
            .handle_message(
                session_id,
                SessionMsg::UserCommand(UserCommand {
                    text: "continue".to_string(),
                }),
            )
            .await;

        assert!(matches!(result, Ok(SessionState::Failed { .. })));
        let persisted = repository.load_state(session_id).await.expect("load state");
        assert!(matches!(persisted, Some(SessionState::Failed { .. })));
        assert!(runtime.calls.lock().await.len() == 1);
        let observer_calls = observer.calls.lock().await.clone();
        assert_eq!(observer_calls.len(), 1);
        assert!(matches!(
            observer_calls[0],
            (
                SessionMsg::UserCommand(UserCommand { .. }),
                SessionState::Failed { .. }
            )
        ));
    }

    #[tokio::test]
    async fn actor_persists_failed_state_when_recover_runtime_dispatch_fails_from_starting() {
        let repository = Arc::new(TestRepository::default());
        let runtime = Arc::new(FailingRuntime::default());
        let actor = SessionActor::new(Arc::clone(&repository), Arc::clone(&runtime), Arc::new(OnceLock::new()));
        let session_id = SessionId::new();

        let result = actor
            .handle_message(session_id, SessionMsg::Recover)
            .await;

        assert!(matches!(result, Ok(SessionState::Failed { .. })));
        let persisted = repository.load_state(session_id).await.expect("load state");
        assert_eq!(
            persisted,
            Some(SessionState::Failed {
                reason: "runtime dispatch failed".to_string(),
            })
        );
        let calls = runtime.calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, SessionMsg::Recover);
        assert_eq!(calls[0].1, SessionState::Idle);
    }

    #[tokio::test]
    async fn actor_persists_original_state_when_send_key_runtime_dispatch_fails() {
        let repository = Arc::new(TestRepository::default());
        let session_id = SessionId::new();
        let running_state = SessionState::Running {
            active_turn: TurnId::new(),
        };
        repository
            .save_state(session_id, &running_state)
            .await
            .expect("seed state");
        let runtime = Arc::new(FailingRuntime::default());
        let actor = SessionActor::new(Arc::clone(&repository), Arc::clone(&runtime), Arc::new(OnceLock::new()));

        let result = actor
            .handle_message(
                session_id,
                SessionMsg::SendKey {
                    key: "Escape".to_string(),
                },
            )
            .await;

        assert_eq!(result.expect("handle send key"), running_state);
        let persisted = repository.load_state(session_id).await.expect("load state");
        assert_eq!(persisted, Some(running_state));
        assert!(runtime.calls.lock().await.len() == 1);
    }

    #[tokio::test]
    async fn actor_clears_runtime_bookkeeping_after_terminal_state_is_persisted() {
        let repository = TestRepository::default();
        let runtime = TestRuntimeWithCleanup::default();
        let actor = SessionActor::new(repository, runtime.clone(), Arc::new(OnceLock::new()));
        let session_id = SessionId::new();

        let next = actor
            .handle_message(session_id, SessionMsg::Terminate)
            .await
            .expect("terminate session");

        assert_eq!(next, SessionState::Completed);
        assert_eq!(runtime.cleanup.cleaned.lock().await.as_slice(), &[session_id]);
    }

    #[tokio::test]
    async fn actor_does_not_forward_stale_user_command_after_terminate() {
        let repository = TestRepository::default();
        let runtime = TestRuntime::default();
        let actor = SessionActor::new(repository, runtime.clone(), Arc::new(OnceLock::new()));
        let session_id = SessionId::new();

        actor
            .handle_message(session_id, SessionMsg::Terminate)
            .await
            .expect("terminate session");
        runtime.calls.lock().await.clear();

        let next = actor
            .handle_message(
                session_id,
                SessionMsg::UserCommand(UserCommand {
                    text: "stale command".to_string(),
                }),
            )
            .await
            .expect("handle stale command");

        assert_eq!(next, SessionState::Completed);
        assert!(runtime.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn actor_does_not_forward_stale_send_key_after_terminate() {
        let repository = TestRepository::default();
        let runtime = TestRuntime::default();
        let actor = SessionActor::new(repository, runtime.clone(), Arc::new(OnceLock::new()));
        let session_id = SessionId::new();

        actor
            .handle_message(session_id, SessionMsg::Terminate)
            .await
            .expect("terminate session");
        runtime.calls.lock().await.clear();

        let next = actor
            .handle_message(
                session_id,
                SessionMsg::SendKey {
                    key: "Escape".to_string(),
                },
            )
            .await
            .expect("handle stale key");

        assert_eq!(next, SessionState::Completed);
        assert!(runtime.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn registry_evicts_terminal_session_handle_after_terminate() {
        let repository = Arc::new(TestRepository::default());
        let runtime = Arc::new(TestRuntime::default());
        let registry = SessionRegistry::new(Arc::clone(&repository), Arc::clone(&runtime));
        let session_id = SessionId::new();

        let first = registry.session(session_id).await;
        let terminated = first
            .send(SessionMsg::Terminate)
            .await
            .expect("terminate session");

        assert_eq!(terminated, SessionState::Completed);
        registry.evict_handle_if_terminal(session_id, &terminated).await;
        assert!(!registry.handles.lock().await.contains_key(&session_id));
    }

    #[tokio::test]
    async fn registry_does_not_cache_new_handle_for_terminal_session() {
        let repository = Arc::new(TestRepository::default());
        let session_id = SessionId::new();
        repository
            .save_state(session_id, &SessionState::Completed)
            .await
            .expect("seed completed state");
        let runtime = Arc::new(TestRuntime::default());
        let registry = SessionRegistry::new(Arc::clone(&repository), Arc::clone(&runtime));

        let next = registry
            .send_to_session(
                session_id,
                SessionMsg::UserCommand(UserCommand {
                    text: "stale".to_string(),
                }),
            )
            .await
            .expect("send stale command");

        assert_eq!(next, SessionState::Completed);
        assert!(registry.handles.lock().await.is_empty());
    }

    #[tokio::test]
    async fn registry_reuses_handle_for_same_session() {
        let registry = SessionRegistry::new(
            Arc::new(TestRepository::default()),
            Arc::new(TestRuntime::default()),
        );
        let session_id = SessionId::new();

        let first = registry.session(session_id).await;
        let second = registry.session(session_id).await;

        assert_eq!(first.session_id(), second.session_id());
    }

    #[tokio::test]
    async fn registry_processes_different_sessions_independently() {
        let repository = Arc::new(TestRepository::default());
        let runtime = Arc::new(TestRuntime::default());
        let registry = SessionRegistry::new(Arc::clone(&repository), Arc::clone(&runtime));
        let first_session = registry.session(SessionId::new()).await;
        let second_session = registry.session(SessionId::new()).await;

        let first_state = first_session
            .send(SessionMsg::UserCommand(UserCommand {
                text: "continue session one".to_string(),
            }))
            .await
            .expect("first session command");
        let second_state = second_session
            .send(SessionMsg::UserCommand(UserCommand {
                text: "continue session two".to_string(),
            }))
            .await
            .expect("second session command");

        assert!(matches!(first_state, SessionState::Running { .. }));
        assert!(matches!(second_state, SessionState::Running { .. }));
        let calls = runtime.calls.lock().await.clone();
        assert_eq!(calls.len(), 2);
        assert!(matches!(calls[0].1, SessionState::Running { .. }));
        assert!(matches!(calls[1].1, SessionState::Running { .. }));
    }

    #[tokio::test]
    async fn registry_actor_persists_state_before_replying() {
        let repository = Arc::new(TestRepository::default());
        let runtime = Arc::new(TestRuntime::default());
        let registry = SessionRegistry::new(Arc::clone(&repository), runtime);
        let session_id = SessionId::new();
        let handle = registry.session(session_id).await;

        let state = handle
            .send(SessionMsg::UserCommand(UserCommand {
                text: "continue".to_string(),
            }))
            .await
            .expect("session command");

        let persisted = repository
            .load_state(session_id)
            .await
            .expect("load persisted state");
        assert_eq!(persisted, Some(state));
    }
}
