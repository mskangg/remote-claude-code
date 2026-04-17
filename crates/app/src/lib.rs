use std::{env, fs, path::{Path, PathBuf}, sync::Arc};

use application::{SlackApplicationService, SlackSessionLifecycleObserver};
use anyhow::Context;
use async_trait::async_trait;
use core_model::SessionState;
use core_service::{RuntimeEngine, SessionRegistry, SessionRepository};
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
    if let Some(configured) = env::var_os("RCC_ENV_FILE").map(PathBuf::from) {
        return Some(configured);
    }
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

async fn mark_recovery_failure(
    repository: &SqliteSessionRepository,
    session_id: core_model::SessionId,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    repository
        .save_state(
            session_id,
            &SessionState::Failed {
                reason: format!("recover failed: {error}"),
            },
        )
        .await
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
                    if let Err(error) = self
                        .session_registry
                        .runtime()
                        .handle(session_id, &core_model::SessionMsg::Recover, &SessionState::Idle)
                        .await
                    {
                        mark_recovery_failure(&self.repository, session_id, &error).await?;
                    } else {
                        self.session_registry
                            .runtime()
                            .recover_active_turn(session_id, active_turn)
                            .await;
                    }
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
    let tmux_ok = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    let manifest_path = workspace_root.join("slack").join("app-manifest.json");
    let env_file_path = find_env_file(workspace_root);
    let env_values = env_file_path
        .as_ref()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|raw| {
            raw.lines()
                .filter_map(|line| line.split_once('=').map(|(key, value)| (key.to_string(), value.to_string())))
                .collect::<std::collections::BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let slack_bot_token = env_values.get("SLACK_BOT_TOKEN");
    let slack_app_token = env_values.get("SLACK_APP_TOKEN");
    let slack_signing_secret = env_values.get("SLACK_SIGNING_SECRET");
    let slack_allowed_user_id = env_values.get("SLACK_ALLOWED_USER_ID");

    vec![
        DoctorCheck {
            name: "slack_bot_token",
            ok: slack_bot_token.is_some_and(|value| !value.trim().is_empty()),
            detail: "SLACK_BOT_TOKEN is configured".to_string(),
        },
        DoctorCheck {
            name: "slack_app_token",
            ok: slack_app_token.is_some_and(|value| !value.trim().is_empty()),
            detail: "SLACK_APP_TOKEN is configured".to_string(),
        },
        DoctorCheck {
            name: "slack_signing_secret",
            ok: slack_signing_secret.is_some_and(|value| !value.trim().is_empty()),
            detail: "SLACK_SIGNING_SECRET is configured".to_string(),
        },
        DoctorCheck {
            name: "slack_allowed_user_id",
            ok: slack_allowed_user_id.is_some_and(|value| !value.trim().is_empty()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceCommand {
    Install,
    Start,
    Stop,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Run,
    Doctor,
    Setup,
    Service(ServiceCommand),
    Help,
    Version,
    Invalid(String),
}

pub fn parse_cli_command(args: &[String]) -> CliCommand {
    match args.get(1).map(|value| value.as_str()) {
        None => CliCommand::Run,
        Some("doctor") => CliCommand::Doctor,
        Some("setup") => CliCommand::Setup,
        Some("service") => CliCommand::Service(parse_service_command(args)),
        Some("help") | Some("--help") | Some("-h") => CliCommand::Help,
        Some("version") | Some("--version") | Some("-V") => CliCommand::Version,
        Some(other) => CliCommand::Invalid(other.to_string()),
    }
}

pub fn parse_service_command(args: &[String]) -> ServiceCommand {
    match args.get(2).map(|value| value.as_str()) {
        Some("install") => ServiceCommand::Install,
        Some("start") => ServiceCommand::Start,
        Some("stop") => ServiceCommand::Stop,
        Some("status") | None => ServiceCommand::Status,
        Some(_) => ServiceCommand::Status,
    }
}

#[cfg(test)]
mod tests {
    use core_model::{SessionState, TurnId};
    use core_service::SessionRepository;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;
    use transport_slack::{SlackSessionStart, SlackSocketModeConfig};

    use super::*;

    fn slack_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().expect("lock slack env")
    }

    fn cwd_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().expect("lock cwd")
    }

    #[test]
    fn parse_cli_command_detects_setup() {
        let args = vec!["rcc".to_string(), "setup".to_string()];
        assert_eq!(parse_cli_command(&args), CliCommand::Setup);
    }

    #[test]
    fn parse_cli_command_detects_service_start() {
        let args = vec!["rcc".to_string(), "service".to_string(), "start".to_string()];
        assert_eq!(parse_cli_command(&args), CliCommand::Service(ServiceCommand::Start));
    }

    #[test]
    fn parse_service_command_defaults_to_status() {
        let args = vec!["rcc".to_string(), "service".to_string()];
        assert_eq!(parse_service_command(&args), ServiceCommand::Status);
    }

    #[test]
    fn parse_cli_command_detects_help_flag() {
        let args = vec!["rcc".to_string(), "--help".to_string()];
        assert_eq!(parse_cli_command(&args), CliCommand::Help);
    }

    #[test]
    fn parse_cli_command_detects_version_flag() {
        let args = vec!["rcc".to_string(), "--version".to_string()];
        assert_eq!(parse_cli_command(&args), CliCommand::Version);
    }

    #[test]
    fn parse_cli_command_rejects_unknown_top_level_argument() {
        let args = vec!["rcc".to_string(), "nonsense".to_string()];
        assert_eq!(parse_cli_command(&args), CliCommand::Invalid("nonsense".to_string()));
    }

    #[test]
    fn parse_setup_cli_options_reads_write_slack_artifact_template() {
        let args = vec![
            "rcc".to_string(),
            "setup".to_string(),
            "--write-slack-artifact-template".to_string(),
            "./tmp/slack-artifact.json".to_string(),
            "--slack-config-token".to_string(),
            "xoxa-config-token".to_string(),
        ];

        let options = setup::parse_setup_cli_options(&args);
        assert_eq!(
            options.write_slack_artifact_template,
            Some(std::path::PathBuf::from("./tmp/slack-artifact.json"))
        );
        assert_eq!(
            options.slack_app_configuration_token.as_deref(),
            Some("xoxa-config-token")
        );
        assert!(options.non_interactive);
    }

    #[test]
    fn parse_setup_cli_options_reads_merge_slack_artifact() {
        let args = vec![
            "rcc".to_string(),
            "setup".to_string(),
            "--merge-slack-artifact".to_string(),
            "./tmp/slack-artifact-patch.json".to_string(),
            "--json".to_string(),
        ];

        let options = setup::parse_setup_cli_options(&args);
        assert_eq!(
            options.merge_slack_artifact,
            Some(std::path::PathBuf::from("./tmp/slack-artifact-patch.json"))
        );
        assert!(options.non_interactive);
        assert!(options.json);
    }

    #[test]
    fn write_slack_setup_artifact_template_creates_json_file() {
        let temp_dir = tempdir().expect("tempdir");
        let path = temp_dir.path().join("slack-artifact.json");

        setup::write_slack_setup_artifact_template(&path, &setup::SetupInput::default())
            .expect("write artifact template");

        let written = fs::read_to_string(&path).expect("read artifact template");
        assert!(written.contains("botToken"));
        assert!(written.contains("projectRoot"));
    }

    #[test]
    fn write_slack_setup_artifact_template_prefills_known_values() {
        let temp_dir = tempdir().expect("tempdir");
        let path = temp_dir.path().join("slack-artifact.json");
        let input = setup::SetupInput {
            channel_id: Some("C123".into()),
            project_root: Some("/tmp/project".into()),
            project_label: Some("demo-project".into()),
            ..Default::default()
        };

        setup::write_slack_setup_artifact_template(&path, &input)
            .expect("write artifact template with prefill");

        let written = fs::read_to_string(&path).expect("read artifact template");
        assert!(written.contains("C123"));
        assert!(written.contains("/tmp/project"));
        assert!(written.contains("demo-project"));
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

    #[test]
    fn setup_completion_message_references_installed_command_and_service_flow() {
        let message = setup::format_setup_completion_message(
            std::path::Path::new("/Users/demo/.local/bin/rcc"),
            std::path::Path::new("/Users/demo/.zshrc"),
            std::path::Path::new("/tmp/workspace/.local/install-rcc.sh"),
        );
        assert!(message.contains("rcc"));
        assert!(message.contains("/Users/demo/.local/bin/rcc"));
        assert!(message.contains("/Users/demo/.zshrc"));
        assert!(message.contains("sh /tmp/workspace/.local/install-rcc.sh"));
        assert!(message.contains("rcc service install && rcc service start"));
        assert!(!message.contains("cargo run -p rcc"));
    }

    #[test]
    fn install_path_defaults_to_user_local_bin() {
        let previous = env::var_os("HOME");
        unsafe { env::set_var("HOME", "/Users/demo") };

        let path = setup::default_install_path().expect("install path");

        match previous {
            Some(value) => unsafe { env::set_var("HOME", value) },
            None => unsafe { env::remove_var("HOME") },
        }

        assert_eq!(path, std::path::PathBuf::from("/Users/demo/.local/bin/rcc"));
    }

    #[test]
    fn install_profile_path_prefers_zshrc_for_zsh_shell() {
        let previous_home = env::var_os("HOME");
        let previous_shell = env::var_os("SHELL");
        unsafe {
            env::set_var("HOME", "/Users/demo");
            env::set_var("SHELL", "/bin/zsh");
        }

        let path = setup::default_shell_profile_path().expect("shell profile");

        match previous_home {
            Some(value) => unsafe { env::set_var("HOME", value) },
            None => unsafe { env::remove_var("HOME") },
        }
        match previous_shell {
            Some(value) => unsafe { env::set_var("SHELL", value) },
            None => unsafe { env::remove_var("SHELL") },
        }

        assert_eq!(path, std::path::PathBuf::from("/Users/demo/.zshrc"));
    }

    #[test]
    fn install_script_contains_path_export_and_binary_copy() {
        let script = setup::build_shell_install_script(
            std::path::Path::new("/tmp/build/rcc"),
            std::path::Path::new("/Users/demo/.local/bin/rcc"),
            std::path::Path::new("/Users/demo/.zshrc"),
            std::path::Path::new("/Users/demo/work/project"),
        );

        assert!(script.contains("install -m 755"));
        assert!(script.contains("/tmp/build/rcc"));
        assert!(script.contains("/Users/demo/.local/bin/rcc"));
        assert!(script.contains("export PATH=\"$HOME/.local/bin:$PATH\""));
        assert!(script.contains("/Users/demo/.zshrc"));
    }

    #[test]
    fn installer_script_path_uses_workspace_local_file() {
        let path = setup::pending_install_script_path(std::path::Path::new("/tmp/workspace"));
        assert_eq!(path, std::path::PathBuf::from("/tmp/workspace/.local/install-rcc.sh"));
    }

    #[test]
    fn setup_completion_message_points_to_installer_script_file() {
        let message = setup::format_setup_completion_message(
            std::path::Path::new("/Users/demo/.local/bin/rcc"),
            std::path::Path::new("/Users/demo/.zshrc"),
            std::path::Path::new("/tmp/workspace/.local/install-rcc.sh"),
        );
        assert!(message.contains("/tmp/workspace/.local/install-rcc.sh"));
        assert!(message.contains("sh /tmp/workspace/.local/install-rcc.sh"));
    }

    #[test]
    fn install_script_wraps_workspace_root_and_env_file() {
        let script = setup::build_shell_install_script(
            std::path::Path::new("/tmp/build/rcc"),
            std::path::Path::new("/Users/demo/.local/bin/rcc"),
            std::path::Path::new("/Users/demo/.zshrc"),
            std::path::Path::new("/Users/demo/work/project"),
        );

        assert!(script.contains("cd \"/Users/demo/work/project\""));
        assert!(script.contains("export RCC_PROJECT_ROOT=\"/Users/demo/work/project\""));
        assert!(script.contains("export RCC_ENV_FILE=\"/Users/demo/work/project/.env.local\""));
        assert!(script.contains("export RCC_HOOK_SETTINGS_PATH=\"/Users/demo/.local/share/remote-claude-code/claude-stop-hooks.json\""));
        assert!(script.contains("export RCC_HOOK_SCRIPT_PATH=\"/Users/demo/.local/share/remote-claude-code/hooks/claude-stop-hook.mjs\""));
        assert!(script.contains("exec \"/Users/demo/.local/bin/rcc.bin\" \"$@\""));
    }

    #[test]
    fn release_binary_path_uses_workspace_target_release() {
        let path = setup::release_binary_path(std::path::Path::new("/tmp/workspace"));
        assert_eq!(path, std::path::PathBuf::from("/tmp/workspace/target/release/rcc"));
    }

    #[tokio::test]
    async fn run_setup_non_interactive_builds_release_binary_before_install() {
        let _guard = cwd_lock();
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack/app-manifest.json"), "{}\n").expect("write manifest");
        fs::create_dir_all(workspace_root.join(".claude")).expect("create claude dir");
        fs::write(workspace_root.join(".env.local"), "SLACK_BOT_TOKEN=x\nSLACK_APP_TOKEN=x\nSLACK_SIGNING_SECRET=x\nSLACK_ALLOWED_USER_ID=U123\n").expect("write env");
        fs::create_dir_all(workspace_root.join("data")).expect("create data dir");
        fs::write(
            workspace_root.join("data/channel-projects.json"),
            "[{\"channelId\":\"C123\",\"projectRoot\":\"/tmp/project\",\"projectLabel\":\"demo\"}]",
        )
        .expect("write mapping");
        fs::create_dir_all(workspace_root.join(".local/hooks")).expect("create hooks dir");
        fs::write(
            workspace_root.join(".local/slack-setup-artifact.json"),
            "{\"slack\":{\"botToken\":\"xoxb\",\"signingSecret\":\"sign\",\"appToken\":\"xapp\",\"allowedUserId\":\"U123\"},\"channel\":{\"id\":\"C123\",\"projectRoot\":\"/tmp/project\",\"projectLabel\":\"demo\"}}",
        )
        .expect("write artifact");

        let original_dir = env::current_dir().expect("cwd");
        env::set_current_dir(workspace_root).expect("chdir");
        let config = AppConfig {
            state_db_path: workspace_root.join(".local/state.db"),
            channel_project_store_path: workspace_root.join("data/channel-projects.json"),
            runtime_working_directory: workspace_root.display().to_string(),
            runtime_launch_command: "claude".to_string(),
            runtime_hook_events_directory: workspace_root.join(".local/hooks").display().to_string(),
            runtime_hook_settings_path: workspace_root.join(".claude/claude-stop-hooks.json"),
        };

        let result = setup::run_setup(
            &config,
            &[
                "rcc".to_string(),
                "setup".to_string(),
                "--from-slack-artifact".to_string(),
                ".local/slack-setup-artifact.json".to_string(),
                "--non-interactive".to_string(),
            ],
        )
        .await;
        env::set_current_dir(original_dir).expect("restore cwd");

        let error = result.expect_err("build step is not implemented yet");
        assert!(error.to_string().contains("cargo build --release -p rcc"));
    }

    #[tokio::test]
    async fn execute_setup_runs_installer_script_when_user_confirms() {
        let _guard = slack_env_lock();
        let previous_home = env::var_os("HOME");
        let previous_shell = env::var_os("SHELL");
        let previous_path = env::var_os("PATH");

        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        unsafe {
            env::set_var("HOME", workspace_root);
            env::set_var("SHELL", "/bin/zsh");
            env::set_var("PATH", "/usr/bin:/bin");
        }
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack/app-manifest.json"), "{}\n").expect("write manifest");
        fs::create_dir_all(workspace_root.join(".claude")).expect("create claude dir");
        let bin_dir = workspace_root.join("bin");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        fs::write(bin_dir.join("tmux"), "#!/bin/sh\nexit 0\n").expect("write fake tmux");
        let mut perms = fs::metadata(bin_dir.join("tmux")).expect("tmux metadata").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
            fs::set_permissions(bin_dir.join("tmux"), perms).expect("chmod fake tmux");
        }
        unsafe { env::set_var("PATH", format!("{}:/usr/bin:/bin", bin_dir.display())) };
        fs::create_dir_all(workspace_root.join("target/release")).expect("create release dir");
        fs::write(workspace_root.join("target/release/rcc"), "#!/bin/sh\nexit 0\n").expect("write fake release binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut release_perms = fs::metadata(workspace_root.join("target/release/rcc")).expect("release metadata").permissions();
            release_perms.set_mode(0o755);
            fs::set_permissions(workspace_root.join("target/release/rcc"), release_perms).expect("chmod fake release binary");
        }

        let config = AppConfig {
            state_db_path: workspace_root.join(".local/state.db"),
            channel_project_store_path: workspace_root.join("data/channel-projects.json"),
            runtime_working_directory: workspace_root.display().to_string(),
            runtime_launch_command: "claude".to_string(),
            runtime_hook_events_directory: workspace_root.join(".local/hooks").display().to_string(),
            runtime_hook_settings_path: workspace_root.join(".claude/claude-stop-hooks.json"),
        };

        let input = setup::SetupInput {
            slack_bot_token: Some("xoxb-bot".into()),
            slack_signing_secret: Some("signing-secret".into()),
            slack_app_token: Some("xapp-app".into()),
            slack_allowed_user_id: Some("U123".into()),
            slack_app_configuration_token: None,
            channel_id: Some("C123".into()),
            project_root: Some(workspace_root.display().to_string()),
            project_label: Some("demo-project".into()),
        };

        let mut prompter = setup::FakePrompter::new(vec![setup::FakeAnswer::Prompt("n".into())]);
        let result = setup::execute_setup(&config, workspace_root, input, &mut prompter).await;

        match previous_home {
            Some(value) => unsafe { env::set_var("HOME", value) },
            None => unsafe { env::remove_var("HOME") },
        }
        match previous_shell {
            Some(value) => unsafe { env::set_var("SHELL", value) },
            None => unsafe { env::remove_var("SHELL") },
        }
        match previous_path {
            Some(value) => unsafe { env::set_var("PATH", value) },
            None => unsafe { env::remove_var("PATH") },
        }

        assert!(result.is_ok(), "{result:?}");
        assert!(prompter.output().contains("설치 스크립트를 지금 실행할까요?"));
        assert!(prompter.output().contains("Run this later with: sh"));
    }

    #[test]
    fn setup_result_reports_manual_required_without_marking_failure() {
        let result = setup::SetupOutcome::ManualRequired {
            summary: "Slack app approval is still required".to_string(),
            next_actions: vec!["Open Slack app install page".to_string()],
        };

        assert!(result.is_manual_required());
        assert!(!result.is_failed());
    }

    #[test]
    fn setup_result_reports_blocked_as_terminal_error() {
        let result = setup::SetupOutcome::Blocked {
            reason: "tmux is not available on PATH".to_string(),
        };

        assert!(result.is_blocked());
        assert!(result.is_failed());
    }

    #[test]
    fn slack_artifact_missing_fields_reports_resume_readiness() {
        let artifact = setup::SlackSetupArtifact {
            slack: setup::SlackArtifactValues {
                bot_token: Some("xoxb-ready".into()),
                signing_secret: None,
                app_token: Some("xapp-ready".into()),
                allowed_user_id: None,
                app_configuration_token: None,
                app_id: None,
                oauth_authorize_url: None,
            },
            channel: setup::SlackArtifactChannel {
                id: Some("C123".into()),
                project_root: Some("/tmp/project".into()),
                project_label: None,
            },
        };

        let missing = setup::slack_artifact_missing_fields(&artifact);
        assert_eq!(
            missing,
            vec!["slack_signing_secret", "slack_allowed_user_id", "project_label"]
        );

        let formatted = setup::format_slack_artifact_resume_status(&artifact);
        assert!(formatted.contains("Artifact is not ready to resume setup yet"));
        assert!(formatted.contains("slack_signing_secret"));
    }

    #[test]
    fn slack_artifact_resume_status_reports_ready_when_complete() {
        let artifact = setup::SlackSetupArtifact {
            slack: setup::SlackArtifactValues {
                bot_token: Some("xoxb-ready".into()),
                signing_secret: Some("signing-ready".into()),
                app_token: Some("xapp-ready".into()),
                allowed_user_id: Some("U123".into()),
                app_configuration_token: None,
                app_id: None,
                oauth_authorize_url: None,
            },
            channel: setup::SlackArtifactChannel {
                id: Some("C123".into()),
                project_root: Some("/tmp/project".into()),
                project_label: Some("demo-project".into()),
            },
        };

        let missing = setup::slack_artifact_missing_fields(&artifact);
        assert!(missing.is_empty());

        let formatted = setup::format_slack_artifact_resume_status(&artifact);
        assert!(formatted.contains("Artifact is ready to resume setup"));
    }

    #[test]
    fn slack_artifact_template_placeholders_are_treated_as_missing() {
        let artifact = setup::SlackSetupArtifact {
            slack: setup::SlackArtifactValues {
                bot_token: Some("xoxb-your-bot-token".into()),
                signing_secret: Some("your-signing-secret".into()),
                app_token: Some("xapp-your-app-token".into()),
                allowed_user_id: Some("U12345678".into()),
                app_configuration_token: None,
                app_id: None,
                oauth_authorize_url: None,
            },
            channel: setup::SlackArtifactChannel {
                id: Some("C12345678".into()),
                project_root: Some("/absolute/path/to/your/project".into()),
                project_label: Some("my-project".into()),
            },
        };

        let missing = setup::slack_artifact_missing_fields(&artifact);
        assert_eq!(
            missing,
            vec![
                "slack_bot_token",
                "slack_signing_secret",
                "slack_app_token",
                "slack_allowed_user_id",
                "channel_id",
                "project_root",
                "project_label"
            ]
        );
    }

    #[test]
    fn apply_manifest_create_response_updates_artifact_with_creation_fields() {
        let artifact = setup::SlackSetupArtifact::default();
        let response = setup::SlackManifestCreateResponse {
            app_id: "A123".into(),
            oauth_authorize_url: "https://slack.com/oauth/v2/authorize?client_id=123".into(),
            credentials: setup::SlackManifestCreateCredentials {
                client_id: "111.222".into(),
                client_secret: "secret".into(),
                verification_token: "verification".into(),
                signing_secret: "signing-secret".into(),
            },
        };

        let updated = setup::apply_manifest_create_response(artifact, &response);
        assert_eq!(updated.slack.app_id.as_deref(), Some("A123"));
        assert_eq!(updated.slack.oauth_authorize_url.as_deref(), Some("https://slack.com/oauth/v2/authorize?client_id=123"));
        assert_eq!(updated.slack.signing_secret.as_deref(), Some("signing-secret"));
    }

    #[test]
    fn hard_failure_formats_blocked_setup_outcome() {
        let prerequisites = setup::SetupPrerequisites {
            tmux_ok: false,
            claude_ok: true,
            manifest_ok: true,
            workspace_writable: true,
            env_exists: false,
            mapping_exists: false,
        };

        let outcome =
            setup::blocked_outcome_from_prerequisites(&prerequisites, std::path::Path::new("/tmp/workspace"));

        assert!(matches!(outcome, setup::SetupOutcome::Blocked { .. }));
        assert!(setup::format_setup_outcome(&outcome).contains("tmux is not available on PATH"));
    }

    #[tokio::test]
    async fn interactive_setup_surfaces_manual_required_slack_step_before_prompting() {
        let temp_dir = tempdir().expect("tempdir");
        let workspace_root = temp_dir.path();
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack/app-manifest.json"), "{}\n").expect("write manifest");

        let config = AppConfig {
            state_db_path: workspace_root.join(".local/state.db"),
            channel_project_store_path: workspace_root.join("data/channel-projects.json"),
            runtime_working_directory: workspace_root.display().to_string(),
            runtime_launch_command: "claude".to_string(),
            runtime_hook_events_directory: workspace_root.join(".local/hooks").display().to_string(),
            runtime_hook_settings_path: workspace_root.join(".claude/claude-stop-hooks.json"),
        };

        let mut prompter = setup::FakePrompter::new(vec![setup::FakeAnswer::Confirm]);
        let error = setup::run_setup_with_prompter(
            &config,
            workspace_root,
            setup::SetupInput {
                channel_id: Some("C123".into()),
                project_root: Some(workspace_root.display().to_string()),
                project_label: Some("demo-project".into()),
                ..Default::default()
            },
            &mut prompter,
        )
        .await
        .expect_err("missing values should stop after manual-required guidance");

        assert!(error.to_string().contains("Slack app approval is still required"));
        assert!(error.to_string().contains("projectRoot already prepared"));
        assert!(error.to_string().contains("projectLabel already prepared: demo-project"));
        assert!(error.to_string().contains("channelId already prepared: C123"));
        assert!(error.to_string().contains("One Slack channel maps to one project"));
        assert!(error.to_string().contains("Create or choose the Slack channel for this project"));
        assert!(error
            .to_string()
            .contains(".local/slack-setup-artifact.json"));
        assert!(error.to_string().contains("--from-slack-artifact"));
        assert!(error.to_string().contains("Invite the bot user to the target channel before testing thread replies"));
        assert!(prompter.output().contains("manual-assisted"));
        assert!(workspace_root.join(".local/slack-setup-artifact.json").exists());
        assert!(error
            .to_string()
            .contains(".local/slack-setup-artifact.json"));
        assert!(!prompter.output().contains("SECRET:SLACK_BOT_TOKEN"));
    }

    #[test]
    fn pending_slack_artifact_path_uses_workspace_local_file() {
        let workspace_root = std::path::Path::new("/tmp/demo-workspace");
        let path = setup::pending_slack_artifact_path(workspace_root);

        assert_eq!(
            path,
            workspace_root.join(".local").join("slack-setup-artifact.json")
        );
    }

    #[tokio::test]
    async fn resolve_setup_input_only_prompts_for_missing_fields() {
        let partial = setup::SetupInput {
            slack_bot_token: Some("xoxb-ready".into()),
            slack_signing_secret: None,
            slack_app_token: None,
            slack_allowed_user_id: Some("U123".into()),
            slack_app_configuration_token: None,
            channel_id: None,
            project_root: Some("/tmp/project".into()),
            project_label: None,
        };

        let mut prompter = setup::FakePrompter::new(vec![
            setup::FakeAnswer::Secret("signing-secret".into()),
            setup::FakeAnswer::Secret("xapp-app".into()),
            setup::FakeAnswer::Prompt("demo".into()),
            setup::FakeAnswer::Prompt("C123".into()),
        ]);

        let resolved = setup::resolve_setup_input(partial, false, &mut prompter)
            .await
            .expect("resolve input");

        assert_eq!(resolved.slack_bot_token.as_deref(), Some("xoxb-ready"));
        assert_eq!(resolved.slack_signing_secret.as_deref(), Some("signing-secret"));
        assert_eq!(resolved.channel_id.as_deref(), Some("C123"));
        assert!(!prompter.output().contains("SECRET:SLACK_BOT_TOKEN"));
    }

    #[test]
    fn load_setup_input_from_json_file() {
        let temp_dir = tempdir().expect("create temp dir");
        let path = temp_dir.path().join("setup.json");
        fs::write(
            &path,
            r#"{
  "slack_bot_token": "xoxb-json",
  "slack_signing_secret": "signing-json",
  "slack_app_token": "xapp-json",
  "slack_allowed_user_id": "UJSON",
  "slack_app_configuration_token": "xoxa-json",
  "channel_id": "CJSON",
  "project_root": "/tmp/project",
  "project_label": "json-project"
}"#,
        )
        .expect("write json file");

        let loaded = setup::load_setup_input_from_file(&path).expect("load setup input");
        assert_eq!(loaded.channel_id.as_deref(), Some("CJSON"));
        assert_eq!(loaded.project_label.as_deref(), Some("json-project"));
        assert_eq!(loaded.slack_app_configuration_token.as_deref(), Some("xoxa-json"));
    }

    #[test]
    fn load_slack_setup_artifact_from_json_file() {
        let temp_dir = tempdir().expect("create temp dir");
        let path = temp_dir.path().join("slack-artifact.json");
        fs::write(
            &path,
            r#"{
  "slack": {
    "botToken": "xoxb-artifact",
    "signingSecret": "signing-artifact",
    "appToken": "xapp-artifact",
    "allowedUserId": "UARTIFACT",
    "appConfigurationToken": "xoxa-artifact",
    "appId": "A123",
    "oauthAuthorizeUrl": "https://slack.com/oauth/v2/authorize?..."
  },
  "channel": {
    "id": "CARTIFACT",
    "projectRoot": "/tmp/project",
    "projectLabel": "artifact-project"
  }
}"#,
        )
        .expect("write artifact file");

        let loaded = setup::load_slack_setup_artifact_from_file(&path).expect("load slack setup artifact");
        let merged = setup::apply_slack_setup_artifact(setup::SetupInput::default(), loaded.clone());

        assert_eq!(merged.slack_bot_token.as_deref(), Some("xoxb-artifact"));
        assert_eq!(merged.slack_app_configuration_token.as_deref(), Some("xoxa-artifact"));
        assert_eq!(loaded.slack.app_id.as_deref(), Some("A123"));
        assert_eq!(loaded.slack.oauth_authorize_url.as_deref(), Some("https://slack.com/oauth/v2/authorize?..."));
        assert_eq!(merged.channel_id.as_deref(), Some("CARTIFACT"));
        assert_eq!(merged.project_label.as_deref(), Some("artifact-project"));
    }

    #[test]
    fn load_slack_setup_patch_from_json_file_with_partial_fields() {
        let temp_dir = tempdir().expect("create temp dir");
        let path = temp_dir.path().join("slack-artifact-patch.json");
        fs::write(
            &path,
            r#"{
  "slack": {
    "appToken": "xapp-patch"
  },
  "channel": {
    "projectLabel": "patched-project"
  }
}"#,
        )
        .expect("write patch file");

        let loaded = setup::load_slack_setup_artifact_from_file(&path).expect("load slack patch artifact");

        assert_eq!(loaded.slack.app_token.as_deref(), Some("xapp-patch"));
        assert_eq!(loaded.slack.bot_token, None);
        assert_eq!(loaded.channel.project_label.as_deref(), Some("patched-project"));
        assert_eq!(loaded.channel.id, None);
    }

    #[test]
    fn merge_slack_setup_artifact_file_updates_only_provided_values() {
        let temp_dir = tempdir().expect("create temp dir");
        let path = temp_dir.path().join("slack-artifact.json");
        fs::write(
            &path,
            r#"{
  "slack": {
    "botToken": "xoxb-existing",
    "signingSecret": "signing-existing",
    "appToken": "xapp-existing",
    "allowedUserId": "UEXISTING"
  },
  "channel": {
    "id": "CEXISTING",
    "projectRoot": "/tmp/existing",
    "projectLabel": "existing-project"
  }
}"#,
        )
        .expect("write artifact file");

        setup::merge_slack_setup_artifact_file(
            &path,
            setup::SlackSetupArtifact {
                slack: setup::SlackArtifactValues {
                    bot_token: None,
                    signing_secret: None,
                    app_token: Some("xapp-updated".into()),
                    allowed_user_id: None,
                    app_configuration_token: None,
                    app_id: None,
                    oauth_authorize_url: None,
                },
                channel: setup::SlackArtifactChannel {
                    id: None,
                    project_root: None,
                    project_label: Some("updated-project".into()),
                },
            },
        )
        .expect("merge artifact file");

        let merged = setup::load_slack_setup_artifact_from_file(&path).expect("reload artifact file");
        assert_eq!(merged.slack.bot_token.as_deref(), Some("xoxb-existing"));
        assert_eq!(merged.slack.app_token.as_deref(), Some("xapp-updated"));
        assert_eq!(merged.channel.project_label.as_deref(), Some("updated-project"));
        assert_eq!(merged.channel.project_root.as_deref(), Some("/tmp/existing"));
    }

    #[test]
    fn merge_pending_slack_artifact_uses_workspace_default_path() {
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        let pending_path = workspace_root.join(".local").join("slack-setup-artifact.json");
        fs::create_dir_all(pending_path.parent().expect("parent")).expect("create .local");
        fs::write(
            &pending_path,
            r#"{
  "slack": {
    "botToken": "xoxb-existing",
    "signingSecret": "signing-existing",
    "appToken": "xapp-existing",
    "allowedUserId": "UEXISTING"
  },
  "channel": {
    "id": "CEXISTING",
    "projectRoot": "/tmp/existing",
    "projectLabel": "existing-project"
  }
}"#,
        )
        .expect("seed pending artifact");

        let patch_path = workspace_root.join("patch.json");
        fs::write(
            &patch_path,
            r#"{
  "slack": {
    "appToken": "xapp-browser"
  },
  "channel": {
    "projectLabel": "browser-project"
  }
}"#,
        )
        .expect("write patch artifact");

        setup::merge_pending_slack_artifact(workspace_root, &patch_path)
            .expect("merge pending artifact");

        let merged = setup::load_slack_setup_artifact_from_file(&pending_path).expect("reload pending artifact");
        assert_eq!(merged.slack.bot_token.as_deref(), Some("xoxb-existing"));
        assert_eq!(merged.slack.app_token.as_deref(), Some("xapp-browser"));
        assert_eq!(merged.channel.project_label.as_deref(), Some("browser-project"));
    }

    #[test]
    fn merge_pending_slack_artifact_reports_resume_status() {
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        let pending_path = workspace_root.join(".local").join("slack-setup-artifact.json");
        fs::create_dir_all(pending_path.parent().expect("parent")).expect("create .local");
        fs::write(
            &pending_path,
            r#"{
  "slack": {
    "botToken": "xoxb-existing",
    "signingSecret": "signing-existing",
    "appToken": null,
    "allowedUserId": "UEXISTING"
  },
  "channel": {
    "id": "CEXISTING",
    "projectRoot": "/tmp/existing",
    "projectLabel": "existing-project"
  }
}"#,
        )
        .expect("seed pending artifact");

        let patch_path = workspace_root.join("patch.json");
        fs::write(
            &patch_path,
            r#"{
  "slack": {
    "appToken": "xapp-browser"
  }
}"#,
        )
        .expect("write patch artifact");

        let status = setup::merge_pending_slack_artifact(workspace_root, &patch_path)
            .expect("merge pending artifact with status");

        assert!(status.contains("Artifact is ready to resume setup"));
    }

    #[test]
    fn merge_pending_slack_artifact_reports_resume_status_as_json() {
        let temp_dir = tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        let pending_path = workspace_root.join(".local").join("slack-setup-artifact.json");
        fs::create_dir_all(pending_path.parent().expect("parent")).expect("create .local");
        fs::write(
            &pending_path,
            r#"{
  "slack": {
    "botToken": "xoxb-existing",
    "signingSecret": null,
    "appToken": null,
    "allowedUserId": "UEXISTING"
  },
  "channel": {
    "id": "CEXISTING",
    "projectRoot": "/tmp/existing",
    "projectLabel": "existing-project"
  }
}"#,
        )
        .expect("seed pending artifact");

        let patch_path = workspace_root.join("patch.json");
        fs::write(
            &patch_path,
            r#"{
  "slack": {
    "appToken": "xapp-browser"
  }
}"#,
        )
        .expect("write patch artifact");

        let report = setup::merge_pending_slack_artifact_report(workspace_root, &patch_path)
            .expect("merge pending artifact report");

        assert!(report.contains("\"ready\":false"));
        assert!(report.contains("slack_signing_secret"));
    }

    #[test]
    fn format_merge_pending_slack_artifact_output_uses_json_when_requested() {
        let report = "{\"ready\":true}";
        let output = setup::format_bridge_output(report, true);
        assert_eq!(output, report);

        let text_output = setup::format_bridge_output(report, false);
        assert_eq!(text_output, report);
    }

    #[test]
    fn env_overrides_json_values_for_setup_input() {
        let previous_channel = env::var_os("RCC_SETUP_CHANNEL_ID");
        let previous_config = env::var_os("RCC_SETUP_SLACK_APP_CONFIGURATION_TOKEN");
        unsafe {
            env::set_var("RCC_SETUP_CHANNEL_ID", "CENV");
            env::set_var("RCC_SETUP_SLACK_APP_CONFIGURATION_TOKEN", "xoxa-env");
        };

        let input = setup::apply_setup_env_overrides(setup::SetupInput {
            channel_id: Some("CJSON".into()),
            ..Default::default()
        });

        assert_eq!(input.channel_id.as_deref(), Some("CENV"));
        assert_eq!(input.slack_app_configuration_token.as_deref(), Some("xoxa-env"));

        match previous_channel {
            Some(value) => unsafe { env::set_var("RCC_SETUP_CHANNEL_ID", value) },
            None => unsafe { env::remove_var("RCC_SETUP_CHANNEL_ID") },
        }
        match previous_config {
            Some(value) => unsafe { env::set_var("RCC_SETUP_SLACK_APP_CONFIGURATION_TOKEN", value) },
            None => unsafe { env::remove_var("RCC_SETUP_SLACK_APP_CONFIGURATION_TOKEN") },
        }
    }

    #[tokio::test]
    async fn existing_values_are_not_overwritten_by_prompt_resolution() {
        let mut prompter = setup::FakePrompter::new(vec![]);
        let input = setup::SetupInput {
            slack_bot_token: Some("xoxb-existing".to_string()),
            slack_signing_secret: Some("secret-existing".to_string()),
            slack_app_token: Some("xapp-existing".to_string()),
            slack_allowed_user_id: Some("U123".to_string()),
            slack_app_configuration_token: None,
            channel_id: Some("C123".to_string()),
            project_root: Some("/tmp/project".to_string()),
            project_label: Some("demo".to_string()),
        };

        let resolved = setup::resolve_setup_input(input.clone(), false, &mut prompter)
            .await
            .expect("resolve without prompts");

        assert_eq!(resolved, input);
        assert_eq!(prompter.output(), "");
    }

    #[tokio::test]
    async fn non_interactive_setup_fails_fast_when_required_fields_are_missing() {
        let mut prompter = setup::FakePrompter::new(vec![]);
        let result = setup::resolve_setup_input(
            setup::SetupInput {
                slack_bot_token: Some("xoxb-ready".into()),
                ..Default::default()
            },
            true,
            &mut prompter,
        )
        .await;

        let error = format!("{result:?}");
        assert!(error.contains("automation-first setup"));
        assert!(error.contains("slack_signing_secret"));
    }

    #[tokio::test]
    async fn run_setup_returns_automation_first_error_for_missing_non_interactive_values() {
        let _guard = cwd_lock();
        let temp_dir = tempdir().expect("tempdir");
        let workspace_root = temp_dir.path();
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack/app-manifest.json"), "{}\n").expect("write manifest");

        let config = AppConfig {
            state_db_path: workspace_root.join(".local/state.db"),
            channel_project_store_path: workspace_root.join("data/channel-projects.json"),
            runtime_working_directory: workspace_root.display().to_string(),
            runtime_launch_command: "claude".to_string(),
            runtime_hook_events_directory: workspace_root.join(".local/hooks").display().to_string(),
            runtime_hook_settings_path: workspace_root.join(".claude/claude-stop-hooks.json"),
        };

        let original_dir = env::current_dir().expect("cwd");
        env::set_current_dir(workspace_root).expect("chdir");
        let result = setup::run_setup(
            &config,
            &["rcc".to_string(), "setup".to_string(), "--non-interactive".to_string()],
        )
        .await;
        env::set_current_dir(original_dir).expect("restore cwd");

        let error = result.expect_err("setup should fail without values");
        assert!(error.to_string().contains("automation-first setup"));
    }

    #[derive(Clone)]
    struct FailingManifestApi;

    #[async_trait::async_trait]
    impl setup::SlackManifestApi for FailingManifestApi {
        async fn create_app(
            &self,
            _config_token: &str,
            _manifest_json: &str,
        ) -> anyhow::Result<setup::SlackManifestCreateResponse> {
            anyhow::bail!("invalid_auth")
        }
    }

    #[derive(Clone)]
    struct SuccessfulManifestApi;

    #[async_trait::async_trait]
    impl setup::SlackManifestApi for SuccessfulManifestApi {
        async fn create_app(
            &self,
            _config_token: &str,
            _manifest_json: &str,
        ) -> anyhow::Result<setup::SlackManifestCreateResponse> {
            Ok(setup::SlackManifestCreateResponse {
                app_id: "A123".into(),
                oauth_authorize_url: "https://slack.com/oauth/v2/authorize?client_id=123".into(),
                credentials: setup::SlackManifestCreateCredentials {
                    client_id: "111.222".into(),
                    client_secret: "secret".into(),
                    verification_token: "verification".into(),
                    signing_secret: "signing-secret".into(),
                },
            })
        }
    }

    #[tokio::test]
    async fn manifest_api_failure_falls_back_to_manual_route() {
        let temp_dir = tempdir().expect("tempdir");
        let workspace_root = temp_dir.path();
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack/app-manifest.json"), "{}\n").expect("write manifest");

        let input = setup::SetupInput {
            slack_app_configuration_token: Some("xoxa-test".into()),
            channel_id: Some("C123".into()),
            project_root: Some(workspace_root.display().to_string()),
            project_label: Some("demo-project".into()),
            ..Default::default()
        };
        let mut prompter = setup::FakePrompter::new(vec![setup::FakeAnswer::Confirm]);

        let result = setup::run_setup_with_manifest_api(
            &FailingManifestApi,
            workspace_root,
            input,
            &mut prompter,
        )
        .await;

        let error = result.expect_err("setup should fall back to manual-assisted route");
        assert!(error.to_string().contains("Slack app approval is still required"));
    }

    #[tokio::test]
    async fn manifest_api_success_writes_creation_fields_into_pending_artifact() {
        let temp_dir = tempdir().expect("tempdir");
        let workspace_root = temp_dir.path();
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack/app-manifest.json"), "{}\n").expect("write manifest");

        let input = setup::SetupInput {
            slack_app_configuration_token: Some("xoxa-test".into()),
            channel_id: Some("C123".into()),
            project_root: Some(workspace_root.display().to_string()),
            project_label: Some("demo-project".into()),
            ..Default::default()
        };
        let mut prompter = setup::FakePrompter::new(vec![setup::FakeAnswer::Confirm]);

        let result = setup::run_setup_with_manifest_api(
            &SuccessfulManifestApi,
            workspace_root,
            input,
            &mut prompter,
        )
        .await;

        let error = result.expect_err("setup should still proceed through manual token collection");
        assert!(error.to_string().contains("Slack app approval is still required"));

        let artifact = setup::load_slack_setup_artifact_from_file(
            &workspace_root.join(".local").join("slack-setup-artifact.json"),
        )
        .expect("load artifact");
        assert_eq!(artifact.slack.app_id.as_deref(), Some("A123"));
        assert_eq!(artifact.slack.oauth_authorize_url.as_deref(), Some("https://slack.com/oauth/v2/authorize?client_id=123"));
        assert_eq!(artifact.slack.signing_secret.as_deref(), Some("signing-secret"));
    }

    #[test]
    fn manifest_api_request_path_uses_form_encoding_contract() {
        let form = setup::build_manifest_create_form_body("xoxe-token", "{\"display_information\":{}}")
            .expect("build form body");

        assert!(form.contains("token=xoxe-token"));
        assert!(form.contains("manifest="));
        assert!(!form.contains("Authorization"));
    }

    #[tokio::test]
    async fn run_setup_accepts_slack_artifact_file_in_non_interactive_mode() {
        let _guard = cwd_lock();
        let temp_dir = tempdir().expect("tempdir");
        let workspace_root = temp_dir.path();
        fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
        fs::write(workspace_root.join("slack/app-manifest.json"), "{}\n").expect("write manifest");

        let artifact_path = workspace_root.join("slack-artifact.json");
        fs::write(
            &artifact_path,
            r#"{
  "slack": {
    "botToken": "xoxb-artifact",
    "signingSecret": "signing-artifact",
    "appToken": "xapp-artifact",
    "allowedUserId": "UARTIFACT"
  },
  "channel": {
    "id": "CARTIFACT",
    "projectRoot": "/tmp/project",
    "projectLabel": "artifact-project"
  }
}"#,
        )
        .expect("write artifact file");

        let config = AppConfig {
            state_db_path: workspace_root.join(".local/state.db"),
            channel_project_store_path: workspace_root.join("data/channel-projects.json"),
            runtime_working_directory: workspace_root.display().to_string(),
            runtime_launch_command: "claude".to_string(),
            runtime_hook_events_directory: workspace_root.join(".local/hooks").display().to_string(),
            runtime_hook_settings_path: workspace_root.join(".claude/claude-stop-hooks.json"),
        };

        let original_dir = env::current_dir().expect("cwd");
        env::set_current_dir(workspace_root).expect("chdir");
        let result = setup::run_setup(
            &config,
            &[
                "rcc".to_string(),
                "setup".to_string(),
                "--from-slack-artifact".to_string(),
                artifact_path.display().to_string(),
                "--non-interactive".to_string(),
            ],
        )
        .await;
        env::set_current_dir(original_dir).expect("restore cwd");

        let error = result.expect_err("doctor should still fail on project_root validation or local state");
        assert!(!error.to_string().contains("missing required fields for automation-first setup"));
    }

    #[tokio::test]
    async fn execute_setup_accepts_pre_resolved_input_without_prompting() {
        let _guard = slack_env_lock();
        let previous_bot = env::var_os("SLACK_BOT_TOKEN");
        let previous_signing = env::var_os("SLACK_SIGNING_SECRET");
        let previous_app = env::var_os("SLACK_APP_TOKEN");
        let previous_user = env::var_os("SLACK_ALLOWED_USER_ID");

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

        let input = setup::SetupInput {
            slack_bot_token: Some("xoxb-bot".into()),
            slack_signing_secret: Some("signing-secret".into()),
            slack_app_token: Some("xapp-app".into()),
            slack_allowed_user_id: Some("U123".into()),
            slack_app_configuration_token: None,
            channel_id: Some("C123".into()),
            project_root: Some(workspace_root.display().to_string()),
            project_label: Some("demo-project".into()),
        };

        let mut prompter = setup::FakePrompter::new(vec![]);
        let result = setup::execute_setup(&config, workspace_root, input, &mut prompter).await;

        match previous_bot {
            Some(value) => unsafe { env::set_var("SLACK_BOT_TOKEN", value) },
            None => unsafe { env::remove_var("SLACK_BOT_TOKEN") },
        }
        match previous_signing {
            Some(value) => unsafe { env::set_var("SLACK_SIGNING_SECRET", value) },
            None => unsafe { env::remove_var("SLACK_SIGNING_SECRET") },
        }
        match previous_app {
            Some(value) => unsafe { env::set_var("SLACK_APP_TOKEN", value) },
            None => unsafe { env::remove_var("SLACK_APP_TOKEN") },
        }
        match previous_user {
            Some(value) => unsafe { env::set_var("SLACK_ALLOWED_USER_ID", value) },
            None => unsafe { env::remove_var("SLACK_ALLOWED_USER_ID") },
        }

        assert!(result.is_ok(), "{result:?}");
        assert!(fs::read_to_string(workspace_root.join(".env.local"))
            .unwrap()
            .contains("SLACK_BOT_TOKEN=xoxb-bot"));
    }

    #[test]
    fn config_prefers_explicit_installed_asset_paths() {
        let previous_state = env::var_os("RCC_STATE_DB_PATH");
        let previous_projects = env::var_os("RCC_CHANNEL_PROJECTS_PATH");
        let previous_root = env::var_os("RCC_PROJECT_ROOT");
        let previous_settings = env::var_os("RCC_HOOK_SETTINGS_PATH");
        let previous_events = env::var_os("RCC_HOOK_EVENTS_DIR");
        let previous_env_file = env::var_os("RCC_ENV_FILE");
        unsafe {
            env::set_var("RCC_STATE_DB_PATH", "/opt/rcc/state.db");
            env::set_var("RCC_CHANNEL_PROJECTS_PATH", "/opt/rcc/channel-projects.json");
            env::set_var("RCC_PROJECT_ROOT", "/work/project");
            env::set_var("RCC_HOOK_SETTINGS_PATH", "/opt/rcc/claude-stop-hooks.json");
            env::set_var("RCC_HOOK_EVENTS_DIR", "/opt/rcc/hooks");
            env::set_var("RCC_ENV_FILE", "/work/project/.env.local");
        }

        let config = AppConfig::from_env();

        assert_eq!(config.state_db_path, std::path::PathBuf::from("/opt/rcc/state.db"));
        assert_eq!(config.channel_project_store_path, std::path::PathBuf::from("/opt/rcc/channel-projects.json"));
        assert_eq!(config.runtime_working_directory, "/work/project");
        assert!(config.runtime_launch_command.contains("/opt/rcc/claude-stop-hooks.json"));
        assert_eq!(config.runtime_hook_events_directory, "/opt/rcc/hooks");
        assert_eq!(config.runtime_hook_settings_path, std::path::PathBuf::from("/opt/rcc/claude-stop-hooks.json"));
        let env_file = find_env_file(std::path::Path::new("/work/project"));
        assert_eq!(env_file, Some(std::path::PathBuf::from("/work/project/.env.local")));

        match previous_state {
            Some(value) => unsafe { env::set_var("RCC_STATE_DB_PATH", value) },
            None => unsafe { env::remove_var("RCC_STATE_DB_PATH") },
        }
        match previous_projects {
            Some(value) => unsafe { env::set_var("RCC_CHANNEL_PROJECTS_PATH", value) },
            None => unsafe { env::remove_var("RCC_CHANNEL_PROJECTS_PATH") },
        }
        match previous_root {
            Some(value) => unsafe { env::set_var("RCC_PROJECT_ROOT", value) },
            None => unsafe { env::remove_var("RCC_PROJECT_ROOT") },
        }
        match previous_settings {
            Some(value) => unsafe { env::set_var("RCC_HOOK_SETTINGS_PATH", value) },
            None => unsafe { env::remove_var("RCC_HOOK_SETTINGS_PATH") },
        }
        match previous_events {
            Some(value) => unsafe { env::set_var("RCC_HOOK_EVENTS_DIR", value) },
            None => unsafe { env::remove_var("RCC_HOOK_EVENTS_DIR") },
        }
        match previous_env_file {
            Some(value) => unsafe { env::set_var("RCC_ENV_FILE", value) },
            None => unsafe { env::remove_var("RCC_ENV_FILE") },
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
    async fn recover_active_sessions_marks_failed_sessions_when_runtime_recovery_cannot_start() {
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

        let previous_path = env::var_os("PATH");
        unsafe { env::set_var("PATH", "") };
        let result = app.recover_active_sessions().await;
        match previous_path {
            Some(value) => unsafe { env::set_var("PATH", value) },
            None => unsafe { env::remove_var("PATH") },
        }

        assert!(result.is_ok());
        let persisted = app
            .repository
            .load_state(session_id)
            .await
            .expect("load state after recovery attempt")
            .expect("persisted state");
        assert!(matches!(
            persisted,
            SessionState::Failed { reason } if reason.contains("recover failed:")
        ));
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
        let _guard = slack_env_lock();
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
        fs::write(
            workspace_root.join(".env.local"),
            "SLACK_BOT_TOKEN=xoxb-test\nSLACK_APP_TOKEN=xapp-test\nSLACK_SIGNING_SECRET=signing-test\nSLACK_ALLOWED_USER_ID=U123\n",
        )
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

    #[test]
    fn bundled_hook_settings_do_not_depend_on_relative_env_file() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root")
            .to_path_buf();
        let hooks_path = workspace_root.join(".claude").join("claude-stop-hooks.json");
        let hooks = fs::read_to_string(&hooks_path).expect("read bundled hook settings");

        assert!(!hooks.contains("--env-file=.env.local"));
        assert!(!hooks.contains("./.claude/hooks/claude-stop-hook.mjs"));
        assert!(hooks.contains("RCC_HOOK_SCRIPT_PATH"));
    }

    #[test]
    fn format_doctor_failures_tells_user_to_invite_bot_to_channel() {
        let checks = vec![DoctorCheck {
            name: "channel_project_mapping",
            ok: false,
            detail: "channel project mapping: /tmp/data/channel-projects.json".to_string(),
        }];

        let output = setup::format_setup_doctor_failures(&checks);
        assert!(output.contains("Invite the bot user to the target channel"));
    }
}
