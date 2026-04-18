//! Session state persistence backed by SQLite.
//!
//! [`SqliteSessionRepository`] persists session state and transport bindings
//! (Slack channel/thread → session ID mappings) in WAL-mode SQLite.
//! [`InMemorySessionRepository`] provides an in-process alternative for tests.

use std::{collections::HashMap, path::Path, sync::{Arc, Mutex}};

use async_trait::async_trait;
use core_model::{SessionId, SessionState, TransportBinding, TransportStatusMessage};
use core_service::SessionRepository;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredChannelSession {
    pub session_id: SessionId,
    pub thread_ts: String,
    pub state: SessionState,
}

#[derive(Default)]
pub struct InMemorySessionRepository {
    states: RwLock<HashMap<SessionId, SessionState>>,
}

impl InMemorySessionRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionRepository for InMemorySessionRepository {
    async fn load_state(&self, session_id: SessionId) -> anyhow::Result<Option<SessionState>> {
        Ok(self.states.read().await.get(&session_id).cloned())
    }

    async fn save_state(&self, session_id: SessionId, state: &SessionState) -> anyhow::Result<()> {
        self.states.write().await.insert(session_id, state.clone());
        Ok(())
    }
}

pub struct SqliteSessionRepository {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteSessionRepository {
    pub fn new(database_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let connection = Connection::open(database_path)?;
        connection.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                state_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS transport_bindings (
                project_space_id TEXT NOT NULL,
                session_space_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                PRIMARY KEY (project_space_id, session_space_id)
            );
            CREATE TABLE IF NOT EXISTS transport_status_messages (
                project_space_id TEXT NOT NULL,
                session_space_id TEXT NOT NULL,
                status_message_id TEXT NOT NULL,
                PRIMARY KEY (project_space_id, session_space_id)
            );
            ",
        )?;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn connection_arc(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.connection)
    }

    pub fn save_transport_binding(
        &self,
        binding: &TransportBinding,
        session_id: SessionId,
    ) -> anyhow::Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;

        connection.execute(
            "
            INSERT INTO transport_bindings (project_space_id, session_space_id, session_id)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(project_space_id, session_space_id)
            DO UPDATE SET session_id = excluded.session_id
            ",
            params![
                binding.project_space_id,
                binding.session_space_id,
                session_id.0.to_string()
            ],
        )?;

        Ok(())
    }

    pub fn find_transport_binding_session_id(
        &self,
        binding: &TransportBinding,
    ) -> anyhow::Result<Option<SessionId>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;
        let session_id: Option<String> = connection
            .query_row(
                "
                SELECT session_id
                FROM transport_bindings
                WHERE project_space_id = ?1 AND session_space_id = ?2
                ",
                params![binding.project_space_id, binding.session_space_id],
                |row| row.get(0),
            )
            .optional()?;

        session_id
            .map(|value| {
                uuid::Uuid::parse_str(&value)
                    .map(SessionId)
                    .map_err(anyhow::Error::from)
            })
            .transpose()
    }

    pub fn find_transport_binding(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Option<TransportBinding>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;
        connection
            .query_row(
                "
                SELECT project_space_id, session_space_id
                FROM transport_bindings
                WHERE session_id = ?1
                ",
                params![session_id.0.to_string()],
                |row| {
                    Ok(TransportBinding {
                        project_space_id: row.get(0)?,
                        session_space_id: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn save_transport_status_message(
        &self,
        status: &TransportStatusMessage,
    ) -> anyhow::Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;

        connection.execute(
            "
            INSERT INTO transport_status_messages (project_space_id, session_space_id, status_message_id)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(project_space_id, session_space_id)
            DO UPDATE SET status_message_id = excluded.status_message_id
            ",
            params![
                status.binding.project_space_id,
                status.binding.session_space_id,
                status.status_message_id
            ],
        )?;

        Ok(())
    }

    pub fn find_transport_status_message(
        &self,
        binding: &TransportBinding,
    ) -> anyhow::Result<Option<TransportStatusMessage>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;
        let status_message_id: Option<String> = connection
            .query_row(
                "
                SELECT status_message_id
                FROM transport_status_messages
                WHERE project_space_id = ?1 AND session_space_id = ?2
                ",
                params![binding.project_space_id, binding.session_space_id],
                |row| row.get(0),
            )
            .optional()?;

        Ok(status_message_id.map(|status_message_id| TransportStatusMessage {
            binding: binding.clone(),
            status_message_id,
        }))
    }

    pub fn list_session_ids(&self) -> anyhow::Result<Vec<SessionId>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;
        let mut statement = connection.prepare("SELECT session_id FROM sessions")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;

        rows.map(|row| {
            let value = row?;
            uuid::Uuid::parse_str(&value)
                .map(SessionId)
                .map_err(|error| rusqlite::Error::FromSqlConversionFailure(
                    value.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
    }

    pub fn list_channel_sessions(&self, channel_id: &str) -> anyhow::Result<Vec<StoredChannelSession>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;
        let mut statement = connection.prepare(
            "
            SELECT transport_bindings.session_id, transport_bindings.session_space_id, sessions.state_json
            FROM transport_bindings
            JOIN sessions ON sessions.session_id = transport_bindings.session_id
            WHERE transport_bindings.project_space_id = ?1
            ORDER BY transport_bindings.session_space_id DESC
            ",
        )?;
        let rows = statement.query_map(params![channel_id], |row| {
            let session_id: String = row.get(0)?;
            let thread_ts: String = row.get(1)?;
            let state_json: String = row.get(2)?;
            Ok((session_id, thread_ts, state_json))
        })?;

        rows.map(|row| -> Result<StoredChannelSession, rusqlite::Error> {
            let (session_id, thread_ts, state_json) = row?;
            let session_id = uuid::Uuid::parse_str(&session_id)
                .map(SessionId)
                .map_err(|error| rusqlite::Error::FromSqlConversionFailure(
                    session_id.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                ))?;
            let state = serde_json::from_str::<SessionState>(&state_json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    state_json.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(StoredChannelSession {
                session_id,
                thread_ts,
                state,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
    }
}

#[async_trait]
impl SessionRepository for SqliteSessionRepository {
    async fn load_state(&self, session_id: SessionId) -> anyhow::Result<Option<SessionState>> {
        let conn = self.connection_arc();
        tokio::task::spawn_blocking(move || {
            let connection = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;
            let state_json: Option<String> = connection
                .query_row(
                    "SELECT state_json FROM sessions WHERE session_id = ?1",
                    params![session_id.0.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            state_json
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(Into::into)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))?
    }

    async fn save_state(&self, session_id: SessionId, state: &SessionState) -> anyhow::Result<()> {
        let conn = self.connection_arc();
        let state_json = serde_json::to_string(state)?;
        tokio::task::spawn_blocking(move || {
            let connection = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("sqlite connection lock poisoned"))?;
            connection.execute(
                "
                INSERT INTO sessions (session_id, state_json)
                VALUES (?1, ?2)
                ON CONFLICT(session_id) DO UPDATE SET state_json = excluded.state_json
                ",
                params![session_id.0.to_string(), state_json],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use core_model::{TransportBinding, TransportStatusMessage, TurnId};
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn sqlite_repository_returns_none_for_missing_session() {
        let temp_dir = tempdir().expect("create tempdir");
        let database_path = temp_dir.path().join("state.db");
        let repository = SqliteSessionRepository::new(&database_path).expect("create repository");

        let state = repository
            .load_state(SessionId::new())
            .await
            .expect("load missing state");

        assert_eq!(state, None);
    }

    #[tokio::test]
    async fn sqlite_repository_persists_state_across_reopen() {
        let temp_dir = tempdir().expect("create tempdir");
        let database_path = temp_dir.path().join("state.db");
        let session_id = SessionId::new();
        let state = SessionState::Running {
            active_turn: TurnId::new(),
        };

        let repository = SqliteSessionRepository::new(&database_path).expect("create repository");
        repository
            .save_state(session_id, &state)
            .await
            .expect("save state");
        drop(repository);

        let reopened = SqliteSessionRepository::new(&database_path).expect("reopen repository");
        let loaded = reopened
            .load_state(session_id)
            .await
            .expect("load saved state");

        assert_eq!(loaded, Some(state));
    }

    #[tokio::test]
    async fn sqlite_repository_persists_transport_binding_across_reopen() {
        let temp_dir = tempdir().expect("create tempdir");
        let database_path = temp_dir.path().join("state.db");
        let session_id = SessionId::new();
        let binding = TransportBinding {
            project_space_id: "C123".to_string(),
            session_space_id: "1740.100".to_string(),
        };

        let repository = SqliteSessionRepository::new(&database_path).expect("create repository");
        repository
            .save_transport_binding(&binding, session_id)
            .expect("save binding");
        drop(repository);

        let reopened = SqliteSessionRepository::new(&database_path).expect("reopen repository");
        let loaded = reopened
            .find_transport_binding_session_id(&binding)
            .expect("load binding");

        assert_eq!(loaded, Some(session_id));
    }

    #[tokio::test]
    async fn sqlite_repository_persists_transport_status_message_across_reopen() {
        let temp_dir = tempdir().expect("create tempdir");
        let database_path = temp_dir.path().join("state.db");
        let status = TransportStatusMessage {
            binding: TransportBinding {
                project_space_id: "C123".to_string(),
                session_space_id: "1740.100".to_string(),
            },
            status_message_id: "1740.200".to_string(),
        };

        let repository = SqliteSessionRepository::new(&database_path).expect("create repository");
        repository
            .save_transport_status_message(&status)
            .expect("save transport status message");
        drop(repository);

        let reopened = SqliteSessionRepository::new(&database_path).expect("reopen repository");
        let loaded = reopened
            .find_transport_status_message(&status.binding)
            .expect("load transport status message");

        assert_eq!(loaded, Some(status));
    }

    #[tokio::test]
    async fn sqlite_repository_lists_persisted_session_ids() {
        let temp_dir = tempdir().expect("create tempdir");
        let database_path = temp_dir.path().join("state.db");
        let repository = SqliteSessionRepository::new(&database_path).expect("create repository");
        let first = SessionId::new();
        let second = SessionId::new();

        repository
            .save_state(first, &SessionState::Idle)
            .await
            .expect("save first");
        repository
            .save_state(
                second,
                &SessionState::Running {
                    active_turn: TurnId::new(),
                },
            )
            .await
            .expect("save second");

        let loaded = repository.list_session_ids().expect("list session ids");

        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&first));
        assert!(loaded.contains(&second));
    }

    #[tokio::test]
    async fn sqlite_repository_lists_channel_sessions() {
        let temp_dir = tempdir().expect("create tempdir");
        let database_path = temp_dir.path().join("state.db");
        let repository = SqliteSessionRepository::new(&database_path).expect("create repository");
        let idle_session = SessionId::new();
        let running_session = SessionId::new();

        repository
            .save_state(idle_session, &SessionState::Idle)
            .await
            .expect("save idle");
        repository
            .save_state(
                running_session,
                &SessionState::Running {
                    active_turn: TurnId::new(),
                },
            )
            .await
            .expect("save running");
        repository
            .save_transport_binding(
                &TransportBinding {
                    project_space_id: "C123".to_string(),
                    session_space_id: "1740.100".to_string(),
                },
                idle_session,
            )
            .expect("save idle binding");
        repository
            .save_transport_binding(
                &TransportBinding {
                    project_space_id: "C123".to_string(),
                    session_space_id: "1740.200".to_string(),
                },
                running_session,
            )
            .expect("save running binding");

        let listed = repository
            .list_channel_sessions("C123")
            .expect("list channel sessions");

        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].session_id, running_session);
        assert_eq!(listed[0].thread_ts, "1740.200");
        assert!(matches!(listed[0].state, SessionState::Running { .. }));
        assert_eq!(listed[1].session_id, idle_session);
        assert_eq!(listed[1].thread_ts, "1740.100");
        assert_eq!(listed[1].state, SessionState::Idle);
    }
}
