use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use dotenvy::from_path_override;

use crate::{find_env_file, run_doctor, AppConfig, ChannelProjectRecord, DoctorCheck, JsonChannelProjectStore};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetupInput {
    pub slack_bot_token: Option<String>,
    pub slack_signing_secret: Option<String>,
    pub slack_app_token: Option<String>,
    pub slack_allowed_user_id: Option<String>,
    pub slack_app_configuration_token: Option<String>,
    pub channel_id: Option<String>,
    pub project_root: Option<String>,
    pub project_label: Option<String>,
}

impl SetupInput {
    pub fn missing_fields(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if is_missing_setup_value(self.slack_bot_token.as_deref()) {
            missing.push("slack_bot_token");
        }
        if is_missing_setup_value(self.slack_signing_secret.as_deref()) {
            missing.push("slack_signing_secret");
        }
        if is_missing_setup_value(self.slack_app_token.as_deref()) {
            missing.push("slack_app_token");
        }
        if is_missing_setup_value(self.slack_allowed_user_id.as_deref()) {
            missing.push("slack_allowed_user_id");
        }
        if is_missing_setup_value(self.channel_id.as_deref()) {
            missing.push("channel_id");
        }
        if is_missing_setup_value(self.project_root.as_deref()) {
            missing.push("project_root");
        }
        if is_missing_setup_value(self.project_label.as_deref()) {
            missing.push("project_label");
        }
        missing
    }
}

fn is_missing_setup_value(value: Option<&str>) -> bool {
    let Some(value) = value.map(str::trim) else {
        return true;
    };

    if value.is_empty() {
        return true;
    }

    matches!(
        value,
        "xoxb-your-bot-token"
            | "your-signing-secret"
            | "xapp-your-app-token"
            | "U12345678"
            | "C12345678"
            | "/absolute/path/to/your/project"
            | "my-project"
    )
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupCliOptions {
    pub from_file: Option<PathBuf>,
    pub from_slack_artifact: Option<PathBuf>,
    pub merge_slack_artifact: Option<PathBuf>,
    pub write_slack_artifact_template: Option<PathBuf>,
    pub slack_app_configuration_token: Option<String>,
    pub non_interactive: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlackSetupArtifact {
    #[serde(default)]
    pub slack: SlackArtifactValues,
    #[serde(default)]
    pub channel: SlackArtifactChannel,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlackArtifactValues {
    #[serde(rename = "botToken")]
    pub bot_token: Option<String>,
    #[serde(rename = "signingSecret")]
    pub signing_secret: Option<String>,
    #[serde(rename = "appToken")]
    pub app_token: Option<String>,
    #[serde(rename = "allowedUserId")]
    pub allowed_user_id: Option<String>,
    #[serde(rename = "appConfigurationToken")]
    pub app_configuration_token: Option<String>,
    #[serde(rename = "appId")]
    pub app_id: Option<String>,
    #[serde(rename = "oauthAuthorizeUrl")]
    pub oauth_authorize_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlackArtifactChannel {
    pub id: Option<String>,
    #[serde(rename = "projectRoot")]
    pub project_root: Option<String>,
    #[serde(rename = "projectLabel")]
    pub project_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlackManifestCreateResponse {
    pub app_id: String,
    pub oauth_authorize_url: String,
    pub credentials: SlackManifestCreateCredentials,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlackManifestCreateCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub verification_token: String,
    pub signing_secret: String,
}

#[async_trait]
pub trait SlackManifestApi {
    async fn create_app(
        &self,
        config_token: &str,
        manifest_json: &str,
    ) -> Result<SlackManifestCreateResponse>;
}

pub struct ReqwestSlackManifestApi;

#[async_trait]
impl SlackManifestApi for ReqwestSlackManifestApi {
    async fn create_app(
        &self,
        config_token: &str,
        manifest_json: &str,
    ) -> Result<SlackManifestCreateResponse> {
        let response = reqwest::Client::new()
            .post("https://slack.com/api/apps.manifest.create")
            .form(&[("token", config_token), ("manifest", manifest_json)])
            .send()
            .await
            .context("send apps.manifest.create request")?;

        let body: serde_json::Value = response
            .json()
            .await
            .context("parse apps.manifest.create response")?;

        if body.get("ok").and_then(|value| value.as_bool()) != Some(true) {
            let error = body
                .get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown_error");
            bail!("apps.manifest.create failed: {error}");
        }

        serde_json::from_value(body).context("decode apps.manifest.create success response")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    Completed {
        summary: String,
    },
    ManualRequired {
        summary: String,
        next_actions: Vec<String>,
    },
    Blocked {
        reason: String,
    },
    Failed {
        reason: String,
    },
}

impl SetupOutcome {
    pub fn is_manual_required(&self) -> bool {
        matches!(self, Self::ManualRequired { .. })
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Blocked { .. } | Self::Failed { .. })
    }
}

pub fn parse_setup_cli_options(args: &[String]) -> SetupCliOptions {
    let mut from_file = None;
    let mut from_slack_artifact = None;
    let mut merge_slack_artifact = None;
    let mut write_slack_artifact_template = None;
    let mut slack_app_configuration_token = None;
    let mut non_interactive = false;
    let mut json = false;
    let mut index = 2;

    while index < args.len() {
        match args[index].as_str() {
            "--from-file" => {
                if let Some(next) = args.get(index + 1) {
                    from_file = Some(PathBuf::from(next));
                    non_interactive = true;
                    index += 2;
                } else {
                    break;
                }
            }
            "--from-slack-artifact" => {
                if let Some(next) = args.get(index + 1) {
                    from_slack_artifact = Some(PathBuf::from(next));
                    non_interactive = true;
                    index += 2;
                } else {
                    break;
                }
            }
            "--merge-slack-artifact" => {
                if let Some(next) = args.get(index + 1) {
                    merge_slack_artifact = Some(PathBuf::from(next));
                    non_interactive = true;
                    index += 2;
                } else {
                    break;
                }
            }
            "--write-slack-artifact-template" => {
                if let Some(next) = args.get(index + 1) {
                    write_slack_artifact_template = Some(PathBuf::from(next));
                    non_interactive = true;
                    index += 2;
                } else {
                    break;
                }
            }
            "--slack-config-token" => {
                if let Some(next) = args.get(index + 1) {
                    slack_app_configuration_token = Some(next.clone());
                    non_interactive = true;
                    index += 2;
                } else {
                    break;
                }
            }
            "--non-interactive" => {
                non_interactive = true;
                index += 1;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            _ => {
                index += 1;
            }
        }
    }

    SetupCliOptions {
        from_file,
        from_slack_artifact,
        merge_slack_artifact,
        write_slack_artifact_template,
        slack_app_configuration_token,
        non_interactive,
        json,
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

pub fn write_slack_setup_artifact_template(path: &Path, input: &SetupInput) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&SlackSetupArtifact {
        slack: SlackArtifactValues {
            bot_token: input
                .slack_bot_token
                .clone()
                .or_else(|| Some("xoxb-your-bot-token".to_string())),
            signing_secret: input
                .slack_signing_secret
                .clone()
                .or_else(|| Some("your-signing-secret".to_string())),
            app_token: input
                .slack_app_token
                .clone()
                .or_else(|| Some("xapp-your-app-token".to_string())),
            allowed_user_id: input
                .slack_allowed_user_id
                .clone()
                .or_else(|| Some("U12345678".to_string())),
            app_configuration_token: input.slack_app_configuration_token.clone(),
            app_id: None,
            oauth_authorize_url: None,
        },
        channel: SlackArtifactChannel {
            id: input.channel_id.clone().or_else(|| Some("C12345678".to_string())),
            project_root: input
                .project_root
                .clone()
                .or_else(|| Some("/absolute/path/to/your/project".to_string())),
            project_label: input
                .project_label
                .clone()
                .or_else(|| Some("my-project".to_string())),
        },
    })?;
    fs::write(path, format!("{body}\n"))?;
    Ok(())
}

pub fn load_slack_manifest_json(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("read Slack manifest JSON: {}", path.display()))
}

pub fn build_manifest_create_form_body(config_token: &str, manifest_json: &str) -> Result<String> {
    let encoded = serde_urlencoded::to_string([
        ("token", config_token),
        ("manifest", manifest_json),
    ])
    .context("encode manifest create request body")?;
    Ok(encoded)
}

pub fn load_setup_input_from_file(path: &Path) -> Result<SetupInput> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read setup file: {}", path.display()))?;
    let input: SetupInput = serde_json::from_str(&raw)
        .with_context(|| format!("parse setup file: {}", path.display()))?;
    Ok(input)
}

pub fn load_slack_setup_artifact_from_file(path: &Path) -> Result<SlackSetupArtifact> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read Slack setup artifact: {}", path.display()))?;
    let artifact: SlackSetupArtifact = serde_json::from_str(&raw)
        .with_context(|| format!("parse Slack setup artifact: {}", path.display()))?;
    Ok(artifact)
}

pub fn merge_slack_setup_artifact_file(path: &Path, update: SlackSetupArtifact) -> Result<()> {
    let mut existing = load_slack_setup_artifact_from_file(path)?;

    if let Some(value) = update.slack.bot_token {
        existing.slack.bot_token = Some(value);
    }
    if let Some(value) = update.slack.signing_secret {
        existing.slack.signing_secret = Some(value);
    }
    if let Some(value) = update.slack.app_token {
        existing.slack.app_token = Some(value);
    }
    if let Some(value) = update.slack.allowed_user_id {
        existing.slack.allowed_user_id = Some(value);
    }
    if let Some(value) = update.slack.app_configuration_token {
        existing.slack.app_configuration_token = Some(value);
    }
    if let Some(value) = update.slack.app_id {
        existing.slack.app_id = Some(value);
    }
    if let Some(value) = update.slack.oauth_authorize_url {
        existing.slack.oauth_authorize_url = Some(value);
    }
    if let Some(value) = update.channel.id {
        existing.channel.id = Some(value);
    }
    if let Some(value) = update.channel.project_root {
        existing.channel.project_root = Some(value);
    }
    if let Some(value) = update.channel.project_label {
        existing.channel.project_label = Some(value);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&existing)?;
    fs::write(path, format!("{body}\n"))?;
    Ok(())
}

pub fn merge_pending_slack_artifact(workspace_root: &Path, patch_path: &Path) -> Result<String> {
    let pending_path = pending_slack_artifact_path(workspace_root);
    let update = load_slack_setup_artifact_from_file(patch_path)?;
    merge_slack_setup_artifact_file(&pending_path, update)?;
    let merged = load_slack_setup_artifact_from_file(&pending_path)?;
    Ok(format_slack_artifact_resume_status(&merged))
}

pub fn merge_pending_slack_artifact_report(workspace_root: &Path, patch_path: &Path) -> Result<String> {
    let pending_path = pending_slack_artifact_path(workspace_root);
    let update = load_slack_setup_artifact_from_file(patch_path)?;
    merge_slack_setup_artifact_file(&pending_path, update)?;
    let merged = load_slack_setup_artifact_from_file(&pending_path)?;
    format_slack_artifact_resume_status_json(&merged)
}

pub fn apply_slack_setup_artifact(mut input: SetupInput, artifact: SlackSetupArtifact) -> SetupInput {
    if input.slack_bot_token.is_none() {
        input.slack_bot_token = artifact.slack.bot_token;
    }
    if input.slack_signing_secret.is_none() {
        input.slack_signing_secret = artifact.slack.signing_secret;
    }
    if input.slack_app_token.is_none() {
        input.slack_app_token = artifact.slack.app_token;
    }
    if input.slack_allowed_user_id.is_none() {
        input.slack_allowed_user_id = artifact.slack.allowed_user_id;
    }
    if input.slack_app_configuration_token.is_none() {
        input.slack_app_configuration_token = artifact.slack.app_configuration_token;
    }
    if input.channel_id.is_none() {
        input.channel_id = artifact.channel.id;
    }
    if input.project_root.is_none() {
        input.project_root = artifact.channel.project_root;
    }
    if input.project_label.is_none() {
        input.project_label = artifact.channel.project_label;
    }
    input
}

pub fn slack_artifact_missing_fields(artifact: &SlackSetupArtifact) -> Vec<&'static str> {
    let input = apply_slack_setup_artifact(SetupInput::default(), artifact.clone());
    input.missing_fields()
}

pub fn format_slack_artifact_resume_status(artifact: &SlackSetupArtifact) -> String {
    let missing = slack_artifact_missing_fields(artifact);
    if missing.is_empty() {
        "Artifact is ready to resume setup. Re-run setup with --from-slack-artifact.".to_string()
    } else {
        format!(
            "Artifact is not ready to resume setup yet. Missing: {}",
            missing.join(", ")
        )
    }
}

pub fn format_slack_artifact_resume_status_json(artifact: &SlackSetupArtifact) -> Result<String> {
    let missing = slack_artifact_missing_fields(artifact);
    let ready = missing.is_empty();
    Ok(serde_json::json!({
        "ready": ready,
        "missing": missing,
        "resumeCommand": "cargo run -p rcc -- setup --from-slack-artifact .local/slack-setup-artifact.json --non-interactive"
    })
    .to_string())
}

pub fn format_bridge_output(output: &str, json: bool) -> String {
    if json {
        output.to_string()
    } else {
        output.to_string()
    }
}

pub fn apply_manifest_create_response(
    mut artifact: SlackSetupArtifact,
    response: &SlackManifestCreateResponse,
) -> SlackSetupArtifact {
    artifact.slack.signing_secret = Some(response.credentials.signing_secret.clone());
    artifact.slack.app_id = Some(response.app_id.clone());
    artifact.slack.oauth_authorize_url = Some(response.oauth_authorize_url.clone());
    artifact
}

pub fn apply_setup_env_overrides(mut input: SetupInput) -> SetupInput {
    if let Ok(value) = std::env::var("RCC_SETUP_SLACK_BOT_TOKEN") {
        input.slack_bot_token = Some(value);
    }
    if let Ok(value) = std::env::var("RCC_SETUP_SLACK_SIGNING_SECRET") {
        input.slack_signing_secret = Some(value);
    }
    if let Ok(value) = std::env::var("RCC_SETUP_SLACK_APP_TOKEN") {
        input.slack_app_token = Some(value);
    }
    if let Ok(value) = std::env::var("RCC_SETUP_SLACK_ALLOWED_USER_ID") {
        input.slack_allowed_user_id = Some(value);
    }
    if let Ok(value) = std::env::var("RCC_SETUP_SLACK_APP_CONFIGURATION_TOKEN") {
        input.slack_app_configuration_token = Some(value);
    }
    if let Ok(value) = std::env::var("RCC_SETUP_CHANNEL_ID") {
        input.channel_id = Some(value);
    }
    if let Ok(value) = std::env::var("RCC_SETUP_PROJECT_ROOT") {
        input.project_root = Some(value);
    }
    if let Ok(value) = std::env::var("RCC_SETUP_PROJECT_LABEL") {
        input.project_label = Some(value);
    }
    input
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

pub fn blocked_outcome_from_prerequisites(
    prerequisites: &SetupPrerequisites,
    workspace_root: &Path,
) -> SetupOutcome {
    let mut lines = vec!["setup cannot continue until these prerequisites are fixed:".to_string()];
    if !prerequisites.tmux_ok {
        lines.push("- tmux is not available on PATH".to_string());
    }
    if !prerequisites.claude_ok {
        lines.push("- claude is not available on PATH".to_string());
    }
    if !prerequisites.manifest_ok {
        lines.push(format!(
            "- missing Slack manifest: {}",
            workspace_root.join("slack/app-manifest.json").display()
        ));
    }
    if !prerequisites.workspace_writable {
        lines.push(format!("- workspace is not writable: {}", workspace_root.display()));
    }

    SetupOutcome::Blocked {
        reason: lines.join("\n"),
    }
}

pub fn format_setup_outcome(outcome: &SetupOutcome) -> String {
    match outcome {
        SetupOutcome::Completed { summary } => summary.clone(),
        SetupOutcome::ManualRequired {
            summary,
            next_actions,
        } => {
            let mut lines = vec![summary.clone()];
            for action in next_actions {
                lines.push(format!("- {action}"));
            }
            lines.join("\n")
        }
        SetupOutcome::Blocked { reason } | SetupOutcome::Failed { reason } => reason.clone(),
    }
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
            "channel_project_mapping" => "channel-projects.json 경로와 channelId/projectRoot 값을 다시 확인하세요. Invite the bot user to the target channel before testing thread replies.",
            _ => "출력된 detail을 확인하고 해당 항목을 수정한 뒤 doctor를 다시 실행하세요.",
        };
        lines.push(format!("- {}: {}", check.name, action));
    }
    lines.join("\n")
}

pub fn format_missing_fields_for_automation(missing: &[&'static str]) -> String {
    format!(
        "missing required fields for automation-first setup: {}. Fill them from existing state, --from-file, generated Slack outputs, or RCC_SETUP_*.",
        missing.join(", ")
    )
}

pub fn default_install_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local").join("bin").join("rcc"))
}

pub fn default_shell_profile_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let file_name = if shell.contains("zsh") {
        ".zshrc"
    } else if shell.contains("bash") {
        ".bashrc"
    } else {
        ".profile"
    };
    Ok(PathBuf::from(home).join(file_name))
}

pub fn pending_install_script_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".local").join("install-rcc.sh")
}

pub fn build_shell_install_script(source_binary_path: &Path, install_path: &Path, profile_path: &Path) -> String {
    format!(
        "#!/usr/bin/env sh\nset -eu\nmkdir -p \"{}\"\ninstall -m 755 \"{}\" \"{}\"\nif ! grep -Fq 'export PATH=\"$HOME/.local/bin:$PATH\"' \"{}\" 2>/dev/null; then\n  printf '\nexport PATH=\"$HOME/.local/bin:$PATH\"\n' >> \"{}\"\nfi\nprintf 'Installed rcc to {}\\n'\nprintf 'Open a new shell or run: . {}\\n'\n",
        install_path.parent().map(|path| path.display().to_string()).unwrap_or_else(|| ".".to_string()),
        source_binary_path.display(),
        install_path.display(),
        profile_path.display(),
        profile_path.display(),
        install_path.display(),
        profile_path.display(),
    )
}

pub fn should_run_installer(answer: &str) -> bool {
    let normalized = answer.trim().to_ascii_lowercase();
    normalized.is_empty() || normalized == "y" || normalized == "yes"
}

pub fn run_install_script(path: &Path) -> Result<()> {
    let status = Command::new("sh")
        .arg(path)
        .status()
        .with_context(|| format!("run install script: {}", path.display()))?;
    if status.success() {
        return Ok(());
    }
    bail!("install script failed with status {status}")
}

pub fn format_setup_completion_message(installed_binary_path: &Path, profile_path: &Path, installer_script_path: &Path) -> String {
    format!(
        "Setup complete. Run the generated installer script with `sh {}` to install `rcc` at {} and update {} if needed. After that, use `rcc` for foreground execution or `rcc service install && rcc service start` for background execution.",
        installer_script_path.display(),
        installed_binary_path.display(),
        profile_path.display(),
    )
}

pub fn pending_slack_artifact_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".local").join("slack-setup-artifact.json")
}

pub fn slack_setup_prefill(input: &SetupInput) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(project_root) = input.project_root.as_deref() {
        lines.push(format!("projectRoot already prepared: {project_root}"));
    }
    if let Some(project_label) = input.project_label.as_deref() {
        lines.push(format!("projectLabel already prepared: {project_label}"));
    }
    if let Some(channel_id) = input.channel_id.as_deref() {
        lines.push(format!("channelId already prepared: {channel_id}"));
    }

    if lines.is_empty() {
        lines.push("No Slack-adjacent setup values were prefilled yet.".to_string());
    }

    lines
}

pub fn slack_manual_required_outcome(input: &SetupInput, artifact_path: &Path) -> SetupOutcome {
    let artifact_path = artifact_path.display().to_string();
    let mut next_actions = vec![
        "Create the app from slack/app-manifest.json".to_string(),
        "Install the app to the workspace and collect the generated tokens".to_string(),
        "One Slack channel maps to one project. Confirm which local project you are connecting first".to_string(),
        "Create or choose the Slack channel for this project".to_string(),
        "Invite the bot user to the target channel before testing thread replies".to_string(),
        "After the channel is ready, collect channelId and finish the channel-project mapping".to_string(),
        format!(
            "A prefilled Slack artifact template was written to {}",
            artifact_path
        ),
        format!(
            "Update that file with generated values, then re-run: cargo run -p rcc -- setup --from-slack-artifact {} --non-interactive",
            artifact_path
        ),
        "Re-run setup with generated values or RCC_SETUP_* overrides".to_string(),
    ];
    next_actions.extend(slack_setup_prefill(input));

    SetupOutcome::ManualRequired {
        summary: "Slack app approval is still required before setup can finish automatically.".to_string(),
        next_actions,
    }
}

pub async fn resolve_setup_input(
    mut input: SetupInput,
    non_interactive: bool,
    prompter: &mut dyn SetupPrompter,
) -> Result<SetupInput> {
    if non_interactive {
        let missing = input.missing_fields();
        if !missing.is_empty() {
            bail!(format_missing_fields_for_automation(&missing));
        }
        return Ok(input);
    }

    if input.slack_bot_token.is_none() {
        input.slack_bot_token = Some(prompter.prompt_secret("SLACK_BOT_TOKEN")?);
    }
    if input.slack_signing_secret.is_none() {
        input.slack_signing_secret = Some(prompter.prompt_secret("SLACK_SIGNING_SECRET")?);
    }
    if input.slack_app_token.is_none() {
        input.slack_app_token = Some(prompter.prompt_secret("SLACK_APP_TOKEN")?);
    }
    if input.slack_allowed_user_id.is_none() {
        input.slack_allowed_user_id = Some(prompter.prompt("SLACK_ALLOWED_USER_ID")?);
    }
    if input.project_root.is_none() {
        input.project_root = Some(prompter.prompt("projectRoot")?);
    }
    if input.project_label.is_none() {
        input.project_label = Some(prompter.prompt("projectLabel")?);
    }
    if input.channel_id.is_none() {
        input.channel_id = Some(prompter.prompt("channelId")?);
    }
    Ok(input)
}

pub async fn execute_setup(
    config: &AppConfig,
    workspace_root: &Path,
    input: SetupInput,
    prompter: &mut dyn SetupPrompter,
) -> Result<()> {
    let project_root = input.project_root.as_deref().context("missing project_root")?;
    validate_project_root(project_root)?;

    let env_path = workspace_root.join(".env.local");
    write_env_updates(
        &env_path,
        &[
            ("SLACK_BOT_TOKEN", input.slack_bot_token.as_deref().context("missing slack_bot_token")?),
            ("SLACK_SIGNING_SECRET", input.slack_signing_secret.as_deref().context("missing slack_signing_secret")?),
            ("SLACK_APP_TOKEN", input.slack_app_token.as_deref().context("missing slack_app_token")?),
            ("SLACK_ALLOWED_USER_ID", input.slack_allowed_user_id.as_deref().context("missing slack_allowed_user_id")?),
        ],
    )?;
    let _ = from_path_override(&env_path);

    let store = JsonChannelProjectStore::new(config.channel_project_store_path.clone());
    let mut records = store.load()?;
    upsert_channel_project_record(
        &mut records,
        ChannelProjectRecord {
            channel_id: input.channel_id.context("missing channel_id")?,
            project_root: project_root.to_string(),
            project_label: input.project_label.context("missing project_label")?,
        },
    );
    write_channel_project_records(&config.channel_project_store_path, &records)?;

    let checks = run_doctor(config, workspace_root);
    print_doctor_summary(prompter, &checks);
    if checks.iter().all(|check| check.ok) {
        let install_path = default_install_path()?;
        let profile_path = default_shell_profile_path()?;
        let installer_script_path = pending_install_script_path(workspace_root);
        let installer_script = build_shell_install_script(Path::new("./target/release/rcc"), &install_path, &profile_path);
        if let Some(parent) = install_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = installer_script_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&installer_script_path, installer_script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&installer_script_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&installer_script_path, perms)?;
        }
        prompter.println(&format_setup_completion_message(&install_path, &profile_path, &installer_script_path));
        let answer = prompter.prompt("설치 스크립트를 지금 실행할까요? [Y/n]")?;
        if should_run_installer(&answer) {
            run_install_script(&installer_script_path)?;
            prompter.println("Installer script executed successfully.");
        } else {
            prompter.println(&format!("Run this later with: sh {}", installer_script_path.display()));
        }
        Ok(())
    } else {
        bail!(format_setup_doctor_failures(&checks))
    }
}

pub async fn run_setup_with_manifest_api<A: SlackManifestApi + Sync>(
    api: &A,
    workspace_root: &Path,
    initial_input: SetupInput,
    _prompter: &mut dyn SetupPrompter,
) -> Result<()> {
    let artifact_path = pending_slack_artifact_path(workspace_root);
    write_slack_setup_artifact_template(&artifact_path, &initial_input)?;

    let manifest_json = load_slack_manifest_json(&workspace_root.join("slack").join("app-manifest.json"))?;
    match api
        .create_app(
            initial_input
                .slack_app_configuration_token
                .as_deref()
                .context("missing slack_app_configuration_token")?,
            &manifest_json,
        )
        .await
    {
        Ok(response) => {
            let artifact = load_slack_setup_artifact_from_file(&artifact_path)?;
            let updated = apply_manifest_create_response(artifact, &response);
            let body = serde_json::to_string_pretty(&updated)?;
            fs::write(&artifact_path, format!("{body}\n"))?;
            bail!(format_setup_outcome(&slack_manual_required_outcome(
                &apply_slack_setup_artifact(initial_input, updated),
                &artifact_path,
            )))
        }
        Err(_) => {
            bail!(format_setup_outcome(&slack_manual_required_outcome(
                &initial_input,
                &artifact_path,
            )))
        }
    }
}

pub async fn run_setup_with_prompter(
    config: &AppConfig,
    workspace_root: &Path,
    initial_input: SetupInput,
    prompter: &mut dyn SetupPrompter,
) -> Result<()> {
    let prerequisites = collect_setup_prerequisites(config, workspace_root);
    if prerequisites.has_hard_failure() {
        bail!(format_setup_outcome(&blocked_outcome_from_prerequisites(
            &prerequisites,
            workspace_root,
        )));
    }

    prompter.println("Remote Claude Code automation-first setup을 시작합니다.");
    prompter.println("Slack app 생성은 manual-assisted 단계로 처리합니다.");
    prompter.println("Manifest path: slack/app-manifest.json");
    prompter.println("Slack link: https://api.slack.com/apps?new_app=1");
    prompter.confirm("Slack app 생성 단계로 넘어가려면 Enter를 누르세요.")?;

    let artifact_path = pending_slack_artifact_path(workspace_root);
    write_slack_setup_artifact_template(&artifact_path, &initial_input)?;

    bail!(format_setup_outcome(&slack_manual_required_outcome(
        &initial_input,
        &artifact_path,
    )));
}

pub async fn run_setup(config: &AppConfig, args: &[String]) -> Result<()> {
    let workspace_root = std::env::current_dir().context("read current directory")?;
    let prerequisites = collect_setup_prerequisites(config, &workspace_root);
    if prerequisites.has_hard_failure() {
        bail!(format_setup_outcome(&blocked_outcome_from_prerequisites(
            &prerequisites,
            &workspace_root,
        )));
    }

    let options = parse_setup_cli_options(args);
    let mut prompter = StdioPrompter;

    if let Some(path) = options.merge_slack_artifact.as_ref() {
        let status = if options.json {
            merge_pending_slack_artifact_report(&workspace_root, path)?
        } else {
            merge_pending_slack_artifact(&workspace_root, path)?
        };
        prompter.println(&format!(
            "Merged Slack artifact patch from {} into {}",
            path.display(),
            pending_slack_artifact_path(&workspace_root).display()
        ));
        prompter.println(&format_bridge_output(&status, options.json));
        return Ok(());
    }

    if let Some(path) = options.write_slack_artifact_template.as_ref() {
        let mut input = SetupInput::default();
        if let Some(path) = options.from_file.as_ref() {
            input = load_setup_input_from_file(path)?;
        }
        if let Some(path) = options.from_slack_artifact.as_ref() {
            input = apply_slack_setup_artifact(input, load_slack_setup_artifact_from_file(path)?);
        }
        input = apply_setup_env_overrides(input);

        write_slack_setup_artifact_template(path, &input)?;
        prompter.println(&format!(
            "Slack artifact template written to {}",
            path.display()
        ));
        return Ok(());
    }

    let mut input = SetupInput::default();
    if let Some(path) = options.from_file.as_ref() {
        input = load_setup_input_from_file(path)?;
    }
    if let Some(path) = options.from_slack_artifact.as_ref() {
        input = apply_slack_setup_artifact(input, load_slack_setup_artifact_from_file(path)?);
    }
    if let Some(token) = options.slack_app_configuration_token.as_ref() {
        input.slack_app_configuration_token = Some(token.clone());
    }
    input = apply_setup_env_overrides(input);

    let has_config_token = input.slack_app_configuration_token.is_some();
    if has_config_token {
        prompter.println("Slack app configuration token detected. Trying manifest API app creation first.");
        return run_setup_with_manifest_api(
            &ReqwestSlackManifestApi,
            &workspace_root,
            input,
            &mut prompter,
        )
        .await;
    }

    if !options.non_interactive {
        return run_setup_with_prompter(config, &workspace_root, input, &mut prompter).await;
    }

    let resolved = resolve_setup_input(input, true, &mut prompter).await?;
    execute_setup(config, &workspace_root, resolved, &mut prompter).await
}
