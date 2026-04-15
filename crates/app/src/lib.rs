use std::{env, fs, path::{Path, PathBuf}, sync::Arc};

use application::{SlackApplicationService, SlackSessionLifecycleObserver};
use anyhow::Context;
use async_trait::async_trait;
use core_model::SessionState;
use core_service::{SessionRegistry, SessionRepository};
use runtime_local::{LocalRuntime, LocalRuntimeConfig, SystemTmuxClient};
use serde::{Deserialize, Serialize};
use session_store::SqliteSessionRepository;
use transport_slack::{
    SlackProject, SlackProjectLocator, SlackSocketModeConfig, SlackTransport, SlackWebApiPublisher,
};

pub struct AppConfig {
    pub state_db_path: PathBuf,
    pub channel_project_store_path: PathBuf,
    pub runtime_working_directory: String,
    pub runtime_launch_command: String,
    pub runtime_hook_events_directory: String,
    pub runtime_hook_settings_path: PathBuf,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let workspace_root = resolve_workspace_root();
        let state_db_path = env::var_os("RCC_STATE_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root.join(".local").join("state.db"));
        let runtime_working_directory = env::var("RCC_PROJECT_ROOT")
            .ok()
            .or_else(|| env::current_dir().ok().map(|path| path.display().to_string()))
            .unwrap_or_else(|| ".".to_string());
        let channel_project_store_path = env::var_os("RCC_CHANNEL_PROJECTS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root.join("data").join("channel-projects.json"));
        let runtime_hook_settings_path = env::var_os("RCC_HOOK_SETTINGS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root.join(".claude").join("claude-stop-hooks.json"));
        let runtime_launch_command = env::var("RCC_CLAUDE_COMMAND").unwrap_or_else(|_| {
            format!(
                "claude --settings {} --dangerously-skip-permissions",
                runtime_hook_settings_path.display()
            )
        });
        let runtime_hook_events_directory =
            env::var("RCC_HOOK_EVENTS_DIR").unwrap_or_else(|_| {
                workspace_root.join(".local").join("hooks").display().to_string()
            });

        Self {
            state_db_path,
            channel_project_store_path,
            runtime_working_directory,
            runtime_launch_command,
            runtime_hook_events_directory,
            runtime_hook_settings_path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelProjectRecord {
    #[serde(rename = "channelId")]
    pub channel_id: String,
    #[serde(rename = "projectRoot")]
    pub project_root: String,
    #[serde(rename = "projectLabel")]
    pub project_label: String,
}

pub struct JsonChannelProjectStore {
    path: PathBuf,
}

impl JsonChannelProjectStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> anyhow::Result<Vec<ChannelProjectRecord>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).context("parse channel project mapping"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }
}

#[async_trait]
impl SlackProjectLocator for JsonChannelProjectStore {
    async fn find_project(&self, channel_id: &str) -> anyhow::Result<Option<SlackProject>> {
        Ok(self.load()?.into_iter().find(|record| record.channel_id == channel_id).map(
            |record| SlackProject {
                project_root: record.project_root,
                project_label: record.project_label,
            },
        ))
    }
}

pub struct AppContext {
    pub repository: Arc<SqliteSessionRepository>,
    pub channel_project_store: Arc<JsonChannelProjectStore>,
    pub session_registry: Arc<SessionRegistry<SqliteSessionRepository, LocalRuntime<SystemTmuxClient>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

pub fn resolve_workspace_root() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn find_env_file(workspace_root: &Path) -> Option<PathBuf> {
    let env_file = workspace_root.join(".env.local");
    env_file.exists().then_some(env_file)
}

pub fn build_app(config: AppConfig) -> anyhow::Result<AppContext> {
    if let Some(parent) = config.state_db_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let repository = Arc::new(SqliteSessionRepository::new(&config.state_db_path)?);
    let channel_project_store = Arc::new(JsonChannelProjectStore::new(
        config.channel_project_store_path.clone(),
    ));
    let runtime = Arc::new(LocalRuntime::new(
        SystemTmuxClient,
        LocalRuntimeConfig {
            working_directory: config.runtime_working_directory,
            launch_command: config.runtime_launch_command,
            hook_events_directory: config.runtime_hook_events_directory,
        },
    ));
    let session_registry = Arc::new(SessionRegistry::new(Arc::clone(&repository), runtime));
    session_registry
        .runtime()
        .set_event_sink(session_registry.clone())?;

    Ok(AppContext {
        repository,
        channel_project_store,
        session_registry,
    })
}

impl AppContext {
    pub async fn recover_active_sessions(&self) -> anyhow::Result<()> {
        for session_id in self.repository.list_session_ids()? {
            let Some(state) = self.repository.load_state(session_id).await? else {
                continue;
            };

            match state {
                SessionState::Running { active_turn }
                | SessionState::Cancelling { active_turn } => {
                    self.session_registry
                        .runtime()
                        .recover_active_turn(session_id, active_turn)
                        .await;
                }
                _ => {}
            }
        }

        Ok(())
    }

    pub async fn cleanup_orphan_tmux_sessions(&self) -> anyhow::Result<Vec<String>> {
        self.session_registry
            .runtime()
            .cleanup_orphan_tmux_sessions(&self.repository.list_session_ids()?)
            .await
    }

    pub fn slack_transport(
        &self,
    ) -> SlackTransport<
        SqliteSessionRepository,
        SessionRegistry<SqliteSessionRepository, LocalRuntime<SystemTmuxClient>>,
        LocalRuntime<SystemTmuxClient>,
    > {
        SlackTransport::new(
            Arc::clone(&self.repository),
            Arc::clone(&self.session_registry),
            Arc::clone(self.session_registry.runtime()),
        )
    }

    pub fn slack_socket_mode_config(&self) -> anyhow::Result<SlackSocketModeConfig> {
        SlackSocketModeConfig::from_env()
    }

    pub fn slack_session_coordinator(
        &self,
        config: &SlackSocketModeConfig,
    ) -> anyhow::Result<
        SlackApplicationService<
            SqliteSessionRepository,
            SessionRegistry<SqliteSessionRepository, LocalRuntime<SystemTmuxClient>>,
            LocalRuntime<SystemTmuxClient>,
            JsonChannelProjectStore,
            SlackWebApiPublisher,
        >,
    > {
        let transport = Arc::new(self.slack_transport());
        let project_locator = Arc::clone(&self.channel_project_store);
        let publisher = Arc::new(SlackWebApiPublisher::new(config.bot_token.clone())?);

        Ok(SlackApplicationService::new(
            transport,
            project_locator,
            publisher,
        ))
    }

    pub fn configure_slack_lifecycle_observer(
        &self,
        config: &SlackSocketModeConfig,
    ) -> anyhow::Result<()> {
        let publisher = Arc::new(SlackWebApiPublisher::new(config.bot_token.clone())?);
        self.session_registry
            .set_observer(Arc::new(SlackSessionLifecycleObserver::new(
                Arc::clone(&self.repository),
                publisher,
            )))
    }
}

pub fn run_doctor(config: &AppConfig, workspace_root: &Path) -> Vec<DoctorCheck> {
    let slack_bot_token = env::var("SLACK_BOT_TOKEN").ok();
    let slack_app_token = env::var("SLACK_APP_TOKEN").ok();
    let slack_signing_secret = env::var("SLACK_SIGNING_SECRET").ok();
    let slack_allowed_user_id = env::var("SLACK_ALLOWED_USER_ID").ok();
    let tmux_ok = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    let manifest_path = workspace_root.join("slack").join("app-manifest.json");
    let env_file_path = find_env_file(workspace_root);

    vec![
        DoctorCheck {
            name: "slack_bot_token",
            ok: slack_bot_token.as_deref().is_some_and(|value| !value.trim().is_empty()),
            detail: "SLACK_BOT_TOKEN is configured".to_string(),
        },
        DoctorCheck {
            name: "slack_app_token",
            ok: slack_app_token.as_deref().is_some_and(|value| !value.trim().is_empty()),
            detail: "SLACK_APP_TOKEN is configured".to_string(),
        },
        DoctorCheck {
            name: "slack_signing_secret",
            ok: slack_signing_secret
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            detail: "SLACK_SIGNING_SECRET is configured".to_string(),
        },
        DoctorCheck {
            name: "slack_allowed_user_id",
            ok: slack_allowed_user_id
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            detail: "SLACK_ALLOWED_USER_ID is configured".to_string(),
        },
        DoctorCheck {
            name: "env_file",
            ok: env_file_path.is_some(),
            detail: format!(
                "env file path: {}",
                env_file_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "not found".to_string())
            ),
        },
        DoctorCheck {
            name: "tmux",
            ok: tmux_ok,
            detail: "tmux is available on PATH".to_string(),
        },
        DoctorCheck {
            name: "state_db_parent",
            ok: config
                .state_db_path
                .parent()
                .is_some_and(|parent| parent.exists() || fs::create_dir_all(parent).is_ok()),
            detail: format!("state db path: {}", config.state_db_path.display()),
        },
        DoctorCheck {
            name: "hook_events_parent",
            ok: fs::create_dir_all(&config.runtime_hook_events_directory).is_ok(),
            detail: format!("hook events dir: {}", config.runtime_hook_events_directory),
        },
        DoctorCheck {
            name: "slack_manifest",
            ok: manifest_path.exists(),
            detail: format!("manifest path: {}", manifest_path.display()),
        },
        DoctorCheck {
            name: "channel_project_mapping",
            ok: config.channel_project_store_path.exists(),
            detail: format!(
                "channel project mapping: {}",
                config.channel_project_store_path.display()
            ),
        },
    ]
}

pub mod setup;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Run,
    Doctor,
    Setup,
}

pub fn parse_cli_command(args: &[String]) -> CliCommand {
    match args.get(1).map(|value| value.as_str()) {
        Some("doctor") => CliCommand::Doctor,
        Some("setup") => CliCommand::Setup,
        _ => CliCommand::Run,
    }
}

#[cfg(test)]
mod tests {
    use core_model::{SessionState, TurnId};
    use core_service::SessionRepository;
    use tempfile::tempdir;
    use transport_slack::{SlackSessionStart, SlackSocketModeConfig};

    use super::*;

    #[test]
    fn parse_cli_command_detects_setup() {
        let args = vec!["rcc".to_string(), "setup".to_string()];
        assert_eq!(parse_cli_command(&args), CliCommand::Setup);
    }

    #[test]
    fn setup_prerequisites_report_missing_env_as_soft_gap() {
        let prerequisites = setup::SetupPrerequisites {
            tmux_ok: true,
            claude_ok: true,
            manifest_ok: true,
            workspace_writable: true,
            env_exists: false,
            mapping_exists: false,
        };

        assert!(!prerequisites.has_hard_failure());
        assert_eq!(prerequisites.soft_gaps(), vec!["env_file", "channel_project_mapping"]);
    }

    #[test]
    fn setup_prerequisites_report_missing_tmux_as_hard_failure() {
        let prerequisites = setup::SetupPrerequisites {
            tmux_ok: false,
            claude_ok: true,
            manifest_ok: true,
            workspace_writable: true,
            env_exists: false,
            mapping_exists: false,
        };

        assert!(prerequisites.has_hard_failure());
    }

    #[test]
    fn write_env_file_updates_only_requested_keys() {
        let temp_dir = tempdir().expect("create temp dir");
        let env_path = temp_dir.path().join(".env.local");
        fs::write(&env_path, "EXTRA=value\nSLACK_BOT_TOKEN=old\n").expect("seed env file");

        let updates = vec![
            ("SLACK_BOT_TOKEN", "new-bot-token"),
            ("SLACK_APP_TOKEN", "new-app-token"),
        ];

        setup::write_env_updates(&env_path, &updates).expect("write env updates");
        let written = fs::read_to_string(&env_path).expect("read env file");

        assert!(written.contains("EXTRA=value"));
        assert!(written.contains("SLACK_BOT_TOKEN=new-bot-token"));
        assert!(written.contains("SLACK_APP_TOKEN=new-app-token"));
    }

    #[test]
    fn upsert_channel_project_record_replaces_existing_channel() {
        let mut records = vec![ChannelProjectRecord {
            channel_id: "C123".to_string(),
            project_root: "/tmp/old".to_string(),
            project_label: "old".to_string(),
        }];

        setup::upsert_channel_project_record(
            &mut records,
            ChannelProjectRecord {
                channel_id: "C123".to_string(),
                project_root: "/tmp/new".to_string(),
                project_label: "new".to_string(),
            },
        );

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].project_root, "/tmp/new");
    }

    #[test]
    fn format_doctor_failures_includes_next_actions() {
        let checks = vec![
            DoctorCheck {
                name: "tmux",
                ok: false,
                detail: "tmux is available on PATH".to_string(),
            },
            DoctorCheck {
                name: "channel_project_mapping",
                ok: false,
                detail: "channel project mapping: /tmp/data/channel-projects.json".to_string(),
            },
        ];

        let output = setup::format_setup_doctor_failures(&checks);
        assert!(output.contains("tmux를 설치"));
        assert!(output.contains("channel-projects.json"));
    }

    #[tokio::test]
    async fn setup_flow_guides_slack_bot_onboarding_and_writes_local_files() {
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack/app-manifest.json"), "{}")
            .expect("write manifest");
        fs::create_dir_all(workspace_root.join(".claude")).expect("create claude dir");

        let config = AppConfig {
            state_db_path: workspace_root.join(".local/state.db"),
            channel_project_store_path: workspace_root.join("data/channel-projects.json"),
            runtime_working_directory: workspace_root.display().to_string(),
            runtime_launch_command: "claude --settings .claude/claude-stop-hooks.json --dangerously-skip-permissions".to_string(),
            runtime_hook_events_directory: workspace_root.join(".local/hooks").display().to_string(),
            runtime_hook_settings_path: workspace_root.join(".claude/claude-stop-hooks.json"),
        };

        let mut prompter = setup::FakePrompter::new(vec![
            setup::FakeAnswer::Confirm,
            setup::FakeAnswer::Secret("xoxb-bot".into()),
            setup::FakeAnswer::Secret("signing-secret".into()),
            setup::FakeAnswer::Secret("xapp-app".into()),
            setup::FakeAnswer::Prompt("U123".into()),
            setup::FakeAnswer::Prompt("C123".into()),
            setup::FakeAnswer::Prompt(workspace_root.display().to_string()),
            setup::FakeAnswer::Prompt("demo-project".into()),
        ]);

        let result = setup::run_setup_with_prompter(&config, workspace_root, &mut prompter).await;
        assert!(result.is_ok(), "{result:?}");
        assert!(prompter.output().contains("Create app from manifest"));
        assert!(prompter.output().contains("cargo run -p rcc"));
        assert!(fs::read_to_string(workspace_root.join(".env.local"))
            .unwrap()
            .contains("SLACK_BOT_TOKEN=xoxb-bot"));
        assert!(fs::read_to_string(workspace_root.join("data/channel-projects.json"))
            .unwrap()
            .contains("demo-project"));
    }

    #[test]
    fn config_defaults_to_workspace_local_paths() {
        let previous = env::var_os("RCC_STATE_DB_PATH");
        unsafe { env::remove_var("RCC_STATE_DB_PATH") };

        let config = AppConfig::from_env();

        assert!(config.state_db_path.ends_with(".local/state.db"));
        assert!(config.channel_project_store_path.ends_with("data/channel-projects.json"));
        assert!(!config.runtime_working_directory.is_empty());
        assert!(config.runtime_launch_command.contains("--settings"));
        assert!(config.runtime_hook_events_directory.ends_with(".local/hooks"));
        assert!(config.runtime_hook_settings_path.ends_with(".claude/claude-stop-hooks.json"));

        match previous {
            Some(value) => unsafe { env::set_var("RCC_STATE_DB_PATH", value) },
            None => unsafe { env::remove_var("RCC_STATE_DB_PATH") },
        }
    }

    #[test]
    fn build_app_creates_parent_directory_for_state_db() {
        let temp_dir = tempdir().expect("create temp dir");
        let database_path = temp_dir.path().join("nested").join("state.db");

        let _app = build_app(AppConfig {
            state_db_path: database_path.clone(),
            channel_project_store_path: temp_dir.path().join("channel-projects.json"),
            runtime_working_directory: "/tmp/project".to_string(),
            runtime_launch_command: "claude --dangerously-skip-permissions".to_string(),
            runtime_hook_events_directory: "/tmp/hooks".to_string(),
            runtime_hook_settings_path: temp_dir.path().join(".claude").join("claude-stop-hooks.json"),
        })
        .expect("build app");

        assert!(database_path.parent().expect("parent").exists());
        assert!(database_path.exists());
    }

    #[test]
    fn build_app_exposes_slack_transport_factory() {
        let temp_dir = tempdir().expect("create temp dir");
        let database_path = temp_dir.path().join("nested").join("state.db");
        let app = build_app(AppConfig {
            state_db_path: database_path,
            channel_project_store_path: temp_dir.path().join("channel-projects.json"),
            runtime_working_directory: "/tmp/project".to_string(),
            runtime_launch_command: "claude --dangerously-skip-permissions".to_string(),
            runtime_hook_events_directory: "/tmp/hooks".to_string(),
            runtime_hook_settings_path: temp_dir.path().join(".claude").join("claude-stop-hooks.json"),
        })
        .expect("build app");

        let _transport = app.slack_transport();
    }

    #[test]
    fn build_app_wires_runtime_event_sink() {
        let temp_dir = tempdir().expect("create temp dir");
        let database_path = temp_dir.path().join("nested").join("state.db");
        let app = build_app(AppConfig {
            state_db_path: database_path,
            channel_project_store_path: temp_dir.path().join("channel-projects.json"),
            runtime_working_directory: "/tmp/project".to_string(),
            runtime_launch_command: "claude --dangerously-skip-permissions".to_string(),
            runtime_hook_events_directory: "/tmp/hooks".to_string(),
            runtime_hook_settings_path: temp_dir.path().join(".claude").join("claude-stop-hooks.json"),
        })
        .expect("build app");

        assert!(app.session_registry.runtime().has_event_sink());
    }

    #[tokio::test]
    async fn app_can_start_slack_session_and_persist_binding_and_state() {
        let temp_dir = tempdir().expect("create temp dir");
        let database_path = temp_dir.path().join("nested").join("state.db");
        let app = build_app(AppConfig {
            state_db_path: database_path,
            channel_project_store_path: temp_dir.path().join("channel-projects.json"),
            runtime_working_directory: "/tmp/project".to_string(),
            runtime_launch_command: "claude --dangerously-skip-permissions".to_string(),
            runtime_hook_events_directory: "/tmp/hooks".to_string(),
            runtime_hook_settings_path: temp_dir.path().join(".claude").join("claude-stop-hooks.json"),
        })
        .expect("build app");
        let transport = app.slack_transport();

        let started = transport
            .start_session(SlackSessionStart {
                channel_id: "C321".to_string(),
                thread_ts: "4000.100".to_string(),
            }, "/tmp/project")
            .await
            .expect("start session");

        let bound = app
            .repository
            .find_transport_binding_session_id(&started.binding)
            .expect("binding lookup");
        let persisted = app
            .repository
            .load_state(started.session_id)
            .await
            .expect("load state");

        assert_eq!(bound, Some(started.session_id));
        assert_eq!(persisted, Some(SessionState::Idle));
    }

    #[tokio::test]
    async fn app_recovers_running_sessions_into_runtime_pending_turns() {
        let temp_dir = tempdir().expect("create temp dir");
        let database_path = temp_dir.path().join("nested").join("state.db");
        let app = build_app(AppConfig {
            state_db_path: database_path,
            channel_project_store_path: temp_dir.path().join("channel-projects.json"),
            runtime_working_directory: "/tmp/project".to_string(),
            runtime_launch_command: "claude --dangerously-skip-permissions".to_string(),
            runtime_hook_events_directory: "/tmp/hooks".to_string(),
            runtime_hook_settings_path: temp_dir.path().join(".claude").join("claude-stop-hooks.json"),
        })
        .expect("build app");
        let session_id = core_model::SessionId::new();
        let turn_id = TurnId::new();
        app.repository
            .save_state(session_id, &SessionState::Running { active_turn: turn_id })
            .await
            .expect("save running state");

        app.recover_active_sessions().await.expect("recover active sessions");

        assert_eq!(
            app.session_registry.runtime().current_turn(session_id).await,
            Some(turn_id)
        );
    }

    #[test]
    fn app_reads_slack_socket_mode_config_from_env() {
        let previous_bot = env::var_os("SLACK_BOT_TOKEN");
        let previous_app = env::var_os("SLACK_APP_TOKEN");
        unsafe {
            env::set_var("SLACK_BOT_TOKEN", "xoxb-test");
            env::set_var("SLACK_APP_TOKEN", "xapp-test");
        }

        let temp_dir = tempdir().expect("create temp dir");
        let database_path = temp_dir.path().join("nested").join("state.db");
        let app = build_app(AppConfig {
            state_db_path: database_path,
            channel_project_store_path: temp_dir.path().join("channel-projects.json"),
            runtime_working_directory: "/tmp/project".to_string(),
            runtime_launch_command: "claude --dangerously-skip-permissions".to_string(),
            runtime_hook_events_directory: "/tmp/hooks".to_string(),
            runtime_hook_settings_path: temp_dir.path().join(".claude").join("claude-stop-hooks.json"),
        })
        .expect("build app");
        let config = app
            .slack_socket_mode_config()
            .expect("read slack socket mode config");

        assert_eq!(
            config,
            SlackSocketModeConfig {
                bot_token: "xoxb-test".to_string(),
                app_token: "xapp-test".to_string(),
            }
        );

        match previous_bot {
            Some(value) => unsafe { env::set_var("SLACK_BOT_TOKEN", value) },
            None => unsafe { env::remove_var("SLACK_BOT_TOKEN") },
        }
        match previous_app {
            Some(value) => unsafe { env::set_var("SLACK_APP_TOKEN", value) },
            None => unsafe { env::remove_var("SLACK_APP_TOKEN") },
        }
    }

    #[test]
    fn doctor_reports_expected_local_checks() {
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack").join("app-manifest.json"), "{}")
            .expect("write manifest");
        fs::write(workspace_root.join(".env.local"), "SLACK_BOT_TOKEN=xoxb-test\n")
            .expect("write env");
        fs::create_dir_all(workspace_root.join("data")).expect("create data dir");
        fs::write(
            workspace_root.join("data").join("channel-projects.json"),
            "[]",
        )
        .expect("write channel-project mapping");
        let config = AppConfig {
            state_db_path: workspace_root.join("state").join("state.db"),
            channel_project_store_path: workspace_root.join("data").join("channel-projects.json"),
            runtime_working_directory: "/tmp/project".to_string(),
            runtime_launch_command: "claude --dangerously-skip-permissions".to_string(),
            runtime_hook_events_directory: workspace_root.join("hooks").display().to_string(),
            runtime_hook_settings_path: workspace_root.join(".claude").join("claude-stop-hooks.json"),
        };

        let checks = run_doctor(&config, workspace_root);

        assert!(checks.iter().any(|check| check.name == "env_file" && check.ok));
        assert!(checks.iter().any(|check| check.name == "slack_manifest" && check.ok));
        assert!(checks.iter().any(|check| check.name == "state_db_parent" && check.ok));
        assert!(checks.iter().any(|check| check.name == "hook_events_parent" && check.ok));
        assert!(checks.iter().any(|check| check.name == "channel_project_mapping" && check.ok));
    }

    #[test]
    fn find_env_file_only_uses_workspace_local_env_file() {
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path().join("repo").join(".worktrees").join("remote-claude-code");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let parent_env = temp_dir.path().join("repo").join(".env.local");
        fs::create_dir_all(parent_env.parent().expect("parent")).expect("create parent");
        fs::write(&parent_env, "SLACK_BOT_TOKEN=xoxb-test\n").expect("write env");

        let found = find_env_file(&workspace_root);

        assert_eq!(found, None);
    }

    #[test]
    fn json_channel_project_store_loads_channel_mapping() {
        let temp_dir = tempdir().expect("create temp dir");
        let path = temp_dir.path().join("channel-projects.json");
        fs::write(
            &path,
            r#"[{"channelId":"C123","projectRoot":"/tmp/project","projectLabel":"demo"}]"#,
        )
        .expect("write mapping");
        let store = JsonChannelProjectStore::new(path);

        let loaded = store.load().expect("load mapping");

        assert_eq!(
            loaded,
            vec![ChannelProjectRecord {
                channel_id: "C123".to_string(),
                project_root: "/tmp/project".to_string(),
                project_label: "demo".to_string(),
            }]
        );
    }

    #[test]
    fn bundled_slack_manifest_allows_posting_session_threads_to_public_channels() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root")
            .to_path_buf();
        let manifest_path = workspace_root.join("slack").join("app-manifest.json");
        let manifest = fs::read_to_string(&manifest_path).expect("read bundled manifest");
        let payload: serde_json::Value = serde_json::from_str(&manifest).expect("parse manifest");
        let scopes = payload["oauth_config"]["scopes"]["bot"]
            .as_array()
            .expect("bot scopes array");

        assert!(
            scopes.iter().any(|scope| scope.as_str() == Some("chat:write.public")),
            "bundled manifest must include chat:write.public so `/cc` can create the session thread in mapped public channels"
        );
    }
}
