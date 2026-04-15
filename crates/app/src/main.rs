use std::{env, path::PathBuf, sync::Arc};

use rcc::{build_app, find_env_file, parse_cli_command, resolve_workspace_root, run_doctor, AppConfig, CliCommand};
use rcc::setup::run_setup;
use transport_slack::{serve_socket_mode, SlackSessionOrchestrator};

#[tokio::main]
async fn main() {
    let workspace_root = resolve_workspace_root();
    if let Some(env_file) = find_env_file(&workspace_root) {
        let _ = dotenvy::from_path(env_file);
    }
    let config = AppConfig::from_env();
    let args: Vec<String> = env::args().collect();
    let workspace_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    match parse_cli_command(&args) {
        CliCommand::Doctor => {
            let checks = run_doctor(&config, &workspace_root);
            let all_ok = checks.iter().all(|check| check.ok);

            for check in checks {
                let status = if check.ok { "OK" } else { "FAIL" };
                println!("[{status}] {} - {}", check.name, check.detail);
            }

            if !all_ok {
                std::process::exit(1);
            }
            return;
        }
        CliCommand::Setup => {
            if let Err(error) = run_setup(&config).await {
                eprintln!("failed to complete setup: {error}");
                std::process::exit(1);
            }
            return;
        }
        CliCommand::Run => {}
    }

    match build_app(config) {
        Ok(app) => {
            if let Err(error) = app.recover_active_sessions().await {
                eprintln!("failed to recover active sessions: {error}");
                std::process::exit(1);
            }
            match app.cleanup_orphan_tmux_sessions().await {
                Ok(removed) if !removed.is_empty() => {
                    eprintln!("rcc: removed orphan tmux sessions: {}", removed.join(", "));
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("failed to cleanup orphan tmux sessions: {error}");
                    std::process::exit(1);
                }
            }
            let slack_config = match app.slack_socket_mode_config() {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("failed to read Slack config: {error}");
                    std::process::exit(1);
                }
            };
            let orchestrator: Arc<dyn SlackSessionOrchestrator> =
                Arc::new(match app.slack_session_coordinator(&slack_config) {
                    Ok(coordinator) => coordinator,
                    Err(error) => {
                        eprintln!("failed to build Slack coordinator: {error}");
                        std::process::exit(1);
                    }
                });
            if let Err(error) = app.configure_slack_lifecycle_observer(&slack_config) {
                eprintln!("failed to configure Slack lifecycle observer: {error}");
                std::process::exit(1);
            }

            if let Err(error) = serve_socket_mode(orchestrator, slack_config).await {
                eprintln!("failed to serve Slack socket mode: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("failed to start remote-claude-code: {error}");
            std::process::exit(1);
        }
    }
}
