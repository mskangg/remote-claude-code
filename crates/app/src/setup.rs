use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::Path,
};

use anyhow::{bail, Context, Result};
use dotenvy::from_path_override;

use crate::{find_env_file, run_doctor, AppConfig, ChannelProjectRecord, DoctorCheck, JsonChannelProjectStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPrerequisites {
    pub tmux_ok: bool,
    pub claude_ok: bool,
    pub manifest_ok: bool,
    pub workspace_writable: bool,
    pub env_exists: bool,
    pub mapping_exists: bool,
}

impl SetupPrerequisites {
    pub fn has_hard_failure(&self) -> bool {
        !self.tmux_ok || !self.claude_ok || !self.manifest_ok || !self.workspace_writable
    }

    pub fn soft_gaps(&self) -> Vec<&'static str> {
        let mut gaps = Vec::new();
        if !self.env_exists {
            gaps.push("env_file");
        }
        if !self.mapping_exists {
            gaps.push("channel_project_mapping");
        }
        gaps
    }
}

pub fn collect_setup_prerequisites(config: &AppConfig, workspace_root: &Path) -> SetupPrerequisites {
    let manifest_path = workspace_root.join("slack").join("app-manifest.json");
    let claude_ok = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    SetupPrerequisites {
        tmux_ok: std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false),
        claude_ok,
        manifest_ok: manifest_path.exists(),
        workspace_writable: fs::create_dir_all(workspace_root.join(".local")).is_ok(),
        env_exists: find_env_file(workspace_root).is_some(),
        mapping_exists: config.channel_project_store_path.exists(),
    }
}

pub trait SetupPrompter {
    fn prompt(&mut self, label: &str) -> Result<String>;
    fn prompt_secret(&mut self, label: &str) -> Result<String>;
    fn confirm(&mut self, label: &str) -> Result<()>;
    fn println(&mut self, line: &str);
}

pub struct StdioPrompter;

impl SetupPrompter for StdioPrompter {
    fn prompt(&mut self, label: &str) -> Result<String> {
        print!("{label}: ");
        io::stdout().flush().context("flush stdout")?;
        let mut input = String::new();
        io::stdin().read_line(&mut input).context("read prompt")?;
        Ok(input.trim().to_string())
    }

    fn prompt_secret(&mut self, label: &str) -> Result<String> {
        self.prompt(label)
    }

    fn confirm(&mut self, label: &str) -> Result<()> {
        self.println(label);
        let _ = self.prompt("")?;
        Ok(())
    }

    fn println(&mut self, line: &str) {
        println!("{line}");
    }
}

#[derive(Debug, Clone)]
pub enum FakeAnswer {
    Prompt(String),
    Secret(String),
    Confirm,
}

pub struct FakePrompter {
    answers: Vec<FakeAnswer>,
    cursor: usize,
    lines: Vec<String>,
}

impl FakePrompter {
    pub fn new(answers: Vec<FakeAnswer>) -> Self {
        Self {
            answers,
            cursor: 0,
            lines: Vec::new(),
        }
    }

    pub fn output(&self) -> String {
        self.lines.join("\n")
    }

    fn next_answer(&mut self) -> Result<FakeAnswer> {
        let answer = self
            .answers
            .get(self.cursor)
            .cloned()
            .context("missing fake answer")?;
        self.cursor += 1;
        Ok(answer)
    }
}

impl SetupPrompter for FakePrompter {
    fn prompt(&mut self, label: &str) -> Result<String> {
        self.lines.push(format!("PROMPT:{label}"));
        match self.next_answer()? {
            FakeAnswer::Prompt(value) => Ok(value),
            other => bail!("expected prompt answer, got {other:?}"),
        }
    }

    fn prompt_secret(&mut self, label: &str) -> Result<String> {
        self.lines.push(format!("SECRET:{label}"));
        match self.next_answer()? {
            FakeAnswer::Secret(value) => Ok(value),
            other => bail!("expected secret answer, got {other:?}"),
        }
    }

    fn confirm(&mut self, label: &str) -> Result<()> {
        self.lines.push(label.to_string());
        match self.next_answer()? {
            FakeAnswer::Confirm => Ok(()),
            other => bail!("expected confirm answer, got {other:?}"),
        }
    }

    fn println(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }
}

pub fn write_env_updates(path: &Path, updates: &[(&str, &str)]) -> Result<()> {
    let mut values = BTreeMap::new();
    if path.exists() {
        for line in fs::read_to_string(path)?.lines() {
            if let Some((key, value)) = line.split_once('=') {
                values.insert(key.to_string(), value.to_string());
            }
        }
    }

    for (key, value) in updates {
        values.insert((*key).to_string(), (*value).to_string());
    }

    let body = values
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{body}\n"))?;
    Ok(())
}

pub fn upsert_channel_project_record(records: &mut Vec<ChannelProjectRecord>, next: ChannelProjectRecord) {
    if let Some(existing) = records.iter_mut().find(|record| record.channel_id == next.channel_id) {
        *existing = next;
    } else {
        records.push(next);
    }
}

pub fn write_channel_project_records(path: &Path, records: &[ChannelProjectRecord]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(records)?)?;
    Ok(())
}

pub fn validate_project_root(project_root: &str) -> Result<()> {
    let path = Path::new(project_root);
    if !path.is_absolute() {
        bail!("projectRoot must be an absolute path");
    }
    if !path.is_dir() {
        bail!("projectRoot must point to an existing directory");
    }
    Ok(())
}

pub fn format_hard_failure(prerequisites: &SetupPrerequisites, workspace_root: &Path) -> String {
    let mut lines = vec!["setup cannot continue until these prerequisites are fixed:".to_string()];
    if !prerequisites.tmux_ok {
        lines.push("- tmux is not available on PATH".to_string());
    }
    if !prerequisites.claude_ok {
        lines.push("- claude is not available on PATH".to_string());
    }
    if !prerequisites.manifest_ok {
        lines.push(format!("- missing Slack manifest: {}", workspace_root.join("slack/app-manifest.json").display()));
    }
    if !prerequisites.workspace_writable {
        lines.push(format!("- workspace is not writable: {}", workspace_root.display()));
    }
    lines.join("\n")
}

pub fn print_doctor_summary(prompter: &mut dyn SetupPrompter, checks: &[DoctorCheck]) {
    for check in checks {
        let status = if check.ok { "OK" } else { "FAIL" };
        prompter.println(&format!("[{status}] {} - {}", check.name, check.detail));
    }
}

pub fn format_setup_doctor_failures(checks: &[DoctorCheck]) -> String {
    let mut lines = vec!["Setup completed, but these items still need attention:".to_string()];
    for check in checks.iter().filter(|check| !check.ok) {
        let action = match check.name {
            "tmux" => "tmux를 설치한 뒤 다시 doctor를 실행하세요.",
            "slack_bot_token" | "slack_app_token" | "slack_signing_secret" | "slack_allowed_user_id" => {
                "Slack 설정 페이지에서 값을 다시 확인하고 setup을 다시 실행하세요."
            }
            "channel_project_mapping" => "channel-projects.json 경로와 channelId/projectRoot 값을 다시 확인하세요.",
            _ => "출력된 detail을 확인하고 해당 항목을 수정한 뒤 doctor를 다시 실행하세요.",
        };
        lines.push(format!("- {}: {}", check.name, action));
    }
    lines.join("\n")
}

pub async fn run_setup_with_prompter(
    config: &AppConfig,
    workspace_root: &Path,
    prompter: &mut dyn SetupPrompter,
) -> Result<()> {
    let prerequisites = collect_setup_prerequisites(config, workspace_root);
    if prerequisites.has_hard_failure() {
        bail!(format_hard_failure(&prerequisites, workspace_root));
    }

    prompter.println("Remote Claude Code Slack-first setup을 시작합니다.");
    prompter.println("Slack app은 Create app from manifest로 생성합니다.");
    prompter.println("Manifest path: slack/app-manifest.json");
    prompter.println("Slack link: https://api.slack.com/apps?new_app=1");
    prompter.confirm("Slack app 생성이 끝났으면 Enter를 누르세요.")?;

    let bot_token = prompter.prompt_secret("SLACK_BOT_TOKEN")?;
    let signing_secret = prompter.prompt_secret("SLACK_SIGNING_SECRET")?;
    let app_token = prompter.prompt_secret("SLACK_APP_TOKEN")?;
    let allowed_user_id = prompter.prompt("SLACK_ALLOWED_USER_ID")?;
    let channel_id = prompter.prompt("channelId")?;
    let project_root = prompter.prompt("projectRoot")?;
    let project_label = prompter.prompt("projectLabel")?;

    validate_project_root(&project_root)?;
    let env_path = workspace_root.join(".env.local");
    write_env_updates(
        &env_path,
        &[
            ("SLACK_BOT_TOKEN", &bot_token),
            ("SLACK_SIGNING_SECRET", &signing_secret),
            ("SLACK_APP_TOKEN", &app_token),
            ("SLACK_ALLOWED_USER_ID", &allowed_user_id),
        ],
    )?;
    let _ = from_path_override(&env_path);

    let store = JsonChannelProjectStore::new(config.channel_project_store_path.clone());
    let mut records = store.load()?;
    upsert_channel_project_record(
        &mut records,
        ChannelProjectRecord {
            channel_id,
            project_root,
            project_label,
        },
    );
    write_channel_project_records(&config.channel_project_store_path, &records)?;

    let checks = run_doctor(config, workspace_root);
    print_doctor_summary(prompter, &checks);
    if checks.iter().all(|check| check.ok) {
        prompter.println("Setup complete. You can now run: cargo run -p rcc");
        Ok(())
    } else {
        bail!(format_setup_doctor_failures(&checks))
    }
}

pub async fn run_setup(config: &AppConfig) -> Result<()> {
    let workspace_root = std::env::current_dir().context("read current directory")?;
    let mut prompter = StdioPrompter;
    run_setup_with_prompter(config, &workspace_root, &mut prompter).await
}
