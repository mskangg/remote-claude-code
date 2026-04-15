# Interactive Setup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Slack-first interactive `rcc setup` command that guides Slack bot onboarding, writes local configuration, and finishes by running `doctor` so the project reaches a runnable state.

**Architecture:** Keep `setup` and `doctor` separate: `setup` owns the guided installation UX and configuration writes, while `doctor` remains the source of truth for readiness checks. Implement the installer inside the `rcc` crate by adding a small setup module for prompting, file updates, and result reporting rather than bloating `main.rs`.

**Tech Stack:** Rust (`tokio`, `serde_json`, `dotenvy`, `anyhow`), existing `rcc` CLI entrypoint, Markdown docs

---

## File structure

### Existing files to modify
- `crates/app/src/main.rs` — add `setup` CLI dispatch
- `crates/app/src/lib.rs` — expose setup helpers, shared doctor result formatting, file update helpers if needed
- `README.md` — change install story to `setup → doctor → run`
- `docs/slack-setup.md` — rewrite around the real `setup` flow
- `docs/manual-smoke-test.md` — reflect the installer-created baseline

### New files to create
- `crates/app/src/setup.rs` — interactive installer flow, prompt handling, file write/update logic, output formatting

### Existing files to test/check
- `crates/app/Cargo.toml` — keep dependency surface minimal; add only what setup truly needs
- `data/channel-projects.example.json` — match generated JSON shape
- `slack/app-manifest.json` — installer must reference this exact path
- `docs/superpowers/specs/2026-04-15-interactive-setup-design.md` — source-of-truth spec

---

### Task 1: Add CLI routing for `rcc setup`

**Files:**
- Modify: `crates/app/src/main.rs:1-77`
- Modify: `crates/app/src/lib.rs:1-321`
- Create: `crates/app/src/setup.rs`
- Test: `crates/app/src/lib.rs:323+`

- [ ] **Step 1: Write the failing test for setup command detection**

Add a small parser helper so command routing is testable without invoking stdin.

```rust
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

#[test]
fn parse_cli_command_detects_setup() {
    let args = vec!["rcc".to_string(), "setup".to_string()];
    assert_eq!(parse_cli_command(&args), CliCommand::Setup);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rcc parse_cli_command_detects_setup -- --exact`
Expected: FAIL because `CliCommand` and `parse_cli_command` do not exist yet.

- [ ] **Step 3: Add the minimal command parser and setup module stub**

Add the enum/helper above to `crates/app/src/lib.rs`, export the setup module, and create a minimal async entrypoint in `crates/app/src/setup.rs`.

```rust
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
```

```rust
use anyhow::Result;

use crate::AppConfig;

pub async fn run_setup(_config: &AppConfig) -> Result<()> {
    Ok(())
}
```

- [ ] **Step 4: Wire `main.rs` to call `setup`**

Replace the ad-hoc `doctor` check with the parser helper.

```rust
use rcc::{build_app, find_env_file, parse_cli_command, resolve_workspace_root, run_doctor, AppConfig, CliCommand};
use rcc::setup::run_setup;

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
```

- [ ] **Step 5: Run the focused tests to verify they pass**

Run: `cargo test -p rcc parse_cli_command_detects_setup -- --exact`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/app/src/main.rs crates/app/src/lib.rs crates/app/src/setup.rs
git commit -m "feat: add setup command routing"
```

---

### Task 2: Build the setup data model and prerequisite checks

**Files:**
- Modify: `crates/app/src/setup.rs`
- Modify: `crates/app/src/lib.rs:323+`
- Test: `crates/app/src/lib.rs:323+`

- [ ] **Step 1: Write the failing tests for prerequisite classification**

Add tests for hard-stop and soft-missing states.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPrerequisites {
    pub tmux_ok: bool,
    pub claude_ok: bool,
    pub manifest_ok: bool,
    pub workspace_writable: bool,
    pub env_exists: bool,
    pub mapping_exists: bool,
}

#[test]
fn setup_prerequisites_report_missing_env_as_soft_gap() {
    let prerequisites = SetupPrerequisites {
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
    let prerequisites = SetupPrerequisites {
        tmux_ok: false,
        claude_ok: true,
        manifest_ok: true,
        workspace_writable: true,
        env_exists: false,
        mapping_exists: false,
    };

    assert!(prerequisites.has_hard_failure());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rcc setup_prerequisites_report_missing_env_as_soft_gap -- --exact`
Expected: FAIL because `SetupPrerequisites` does not exist yet.

- [ ] **Step 3: Add the setup prerequisite model and checks**

Implement the model in `crates/app/src/setup.rs`.

```rust
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
```

- [ ] **Step 4: Run both prerequisite tests to verify they pass**

Run: `cargo test -p rcc setup_prerequisites_report_missing_env_as_soft_gap setup_prerequisites_report_missing_tmux_as_hard_failure`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/app/src/setup.rs crates/app/src/lib.rs
git commit -m "feat: add setup prerequisite checks"
```

---

### Task 3: Add interactive prompts and safe config writers

**Files:**
- Modify: `crates/app/src/setup.rs`
- Modify: `crates/app/src/lib.rs:62-92`
- Test: `crates/app/src/lib.rs:323+`

- [ ] **Step 1: Write the failing tests for env and mapping updates**

Add tests for preserving unrelated env keys and updating channel mappings.

```rust
#[test]
fn write_env_file_updates_only_requested_keys() {
    let temp_dir = tempdir().expect("create temp dir");
    let env_path = temp_dir.path().join(".env.local");
    fs::write(&env_path, "EXTRA=value\nSLACK_BOT_TOKEN=old\n").expect("seed env file");

    let updates = vec![
        ("SLACK_BOT_TOKEN", "new-bot-token"),
        ("SLACK_APP_TOKEN", "new-app-token"),
    ];

    write_env_updates(&env_path, &updates).expect("write env updates");
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

    upsert_channel_project_record(
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rcc write_env_file_updates_only_requested_keys -- --exact`
Expected: FAIL because writer helpers do not exist yet.

- [ ] **Step 3: Implement env writer and mapping upsert helpers**

Add focused helpers in `setup.rs`.

```rust
pub fn write_env_updates(path: &Path, updates: &[(&str, &str)]) -> anyhow::Result<()> {
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
```

- [ ] **Step 4: Add a prompt abstraction so setup logic is testable**

Add a minimal trait and one stdio implementation.

```rust
pub trait SetupPrompter {
    fn prompt(&mut self, label: &str) -> anyhow::Result<String>;
    fn prompt_secret(&mut self, label: &str) -> anyhow::Result<String>;
    fn confirm(&mut self, label: &str) -> anyhow::Result<()>;
    fn println(&mut self, line: &str);
}
```

The first implementation can use `stdin.read_line` for normal input and plain input for secrets if hiding is not available yet; do not block the plan on advanced terminal masking.

- [ ] **Step 5: Run the focused tests to verify they pass**

Run: `cargo test -p rcc write_env_file_updates_only_requested_keys upsert_channel_project_record_replaces_existing_channel`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/app/src/setup.rs crates/app/src/lib.rs
git commit -m "feat: add setup config writers"
```

---

### Task 4: Implement the guided Slack bot onboarding flow

**Files:**
- Modify: `crates/app/src/setup.rs`
- Test: `crates/app/src/lib.rs:323+`

- [ ] **Step 1: Write the failing test for setup flow output and writes**

Add a fake prompter and verify the setup flow prints links, writes files, and requests the correct inputs.

```rust
#[tokio::test]
async fn setup_flow_guides_slack_bot_onboarding_and_writes_local_files() {
    let temp_dir = tempdir().expect("create temp dir");
    let workspace_root = temp_dir.path();
    fs::create_dir_all(workspace_root.join("slack")).expect("create slack dir");
    fs::write(workspace_root.join("slack/app-manifest.json"), "{}").expect("write manifest");

    let config = AppConfig {
        state_db_path: workspace_root.join(".local/state.db"),
        channel_project_store_path: workspace_root.join("data/channel-projects.json"),
        runtime_working_directory: workspace_root.display().to_string(),
        runtime_launch_command: "claude --settings .claude/claude-stop-hooks.json --dangerously-skip-permissions".to_string(),
        runtime_hook_events_directory: workspace_root.join(".local/hooks").display().to_string(),
        runtime_hook_settings_path: workspace_root.join(".claude/claude-stop-hooks.json"),
    };

    let mut prompter = FakePrompter::new(vec![
        FakeAnswer::Confirm,
        FakeAnswer::Secret("xoxb-bot".into()),
        FakeAnswer::Secret("signing-secret".into()),
        FakeAnswer::Secret("xapp-app".into()),
        FakeAnswer::Prompt("U123".into()),
        FakeAnswer::Prompt("C123".into()),
        FakeAnswer::Prompt(workspace_root.display().to_string()),
        FakeAnswer::Prompt("demo-project".into()),
    ]);

    let result = run_setup_with_prompter(&config, workspace_root, &mut prompter).await;
    assert!(result.is_ok());
    assert!(prompter.output().contains("Create app from manifest"));
    assert!(fs::read_to_string(workspace_root.join(".env.local")).unwrap().contains("SLACK_BOT_TOKEN=xoxb-bot"));
    assert!(fs::read_to_string(workspace_root.join("data/channel-projects.json")).unwrap().contains("demo-project"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rcc setup_flow_guides_slack_bot_onboarding_and_writes_local_files -- --exact`
Expected: FAIL because the interactive flow helper does not exist yet.

- [ ] **Step 3: Implement the onboarding flow**

Create a testable helper.

```rust
pub async fn run_setup_with_prompter(
    config: &AppConfig,
    workspace_root: &Path,
    prompter: &mut dyn SetupPrompter,
) -> anyhow::Result<()> {
    let prerequisites = collect_setup_prerequisites(config, workspace_root);
    if prerequisites.has_hard_failure() {
        anyhow::bail!(format_hard_failure(&prerequisites, workspace_root));
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
    write_env_updates(
        &workspace_root.join(".env.local"),
        &[
            ("SLACK_BOT_TOKEN", &bot_token),
            ("SLACK_SIGNING_SECRET", &signing_secret),
            ("SLACK_APP_TOKEN", &app_token),
            ("SLACK_ALLOWED_USER_ID", &allowed_user_id),
        ],
    )?;

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
        anyhow::bail!("setup completed, but doctor still reports failures")
    }
}
```

- [ ] **Step 4: Run the focused test to verify it passes**

Run: `cargo test -p rcc setup_flow_guides_slack_bot_onboarding_and_writes_local_files -- --exact`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/app/src/setup.rs crates/app/src/lib.rs crates/app/src/main.rs
git commit -m "feat: add interactive setup flow"
```

---

### Task 5: Add doctor-aware corrective output for setup failures

**Files:**
- Modify: `crates/app/src/setup.rs`
- Test: `crates/app/src/lib.rs:323+`

- [ ] **Step 1: Write the failing test for corrective action output**

```rust
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

    let output = format_setup_doctor_failures(&checks);
    assert!(output.contains("tmux를 설치"));
    assert!(output.contains("channel-projects.json"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rcc format_doctor_failures_includes_next_actions -- --exact`
Expected: FAIL because the formatter does not exist yet.

- [ ] **Step 3: Implement corrective failure formatting**

```rust
pub fn format_setup_doctor_failures(checks: &[DoctorCheck]) -> String {
    let mut lines = vec!["Setup completed, but these items still need attention:".to_string()];
    for check in checks.iter().filter(|check| !check.ok) {
        let action = match check.name {
            "tmux" => "tmux를 설치한 뒤 다시 doctor를 실행하세요.",
            "slack_bot_token" | "slack_app_token" | "slack_signing_secret" | "slack_allowed_user_id" => "Slack 설정 페이지에서 값을 다시 확인하고 setup을 다시 실행하세요.",
            "channel_project_mapping" => "channel-projects.json 경로와 channelId/projectRoot 값을 다시 확인하세요.",
            _ => "출력된 detail을 확인하고 해당 항목을 수정한 뒤 doctor를 다시 실행하세요.",
        };
        lines.push(format!("- {}: {}", check.name, action));
    }
    lines.join("\n")
}
```

Use this message instead of a generic error when setup ends with doctor failures.

- [ ] **Step 4: Run the focused test to verify it passes**

Run: `cargo test -p rcc format_doctor_failures_includes_next_actions -- --exact`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/app/src/setup.rs crates/app/src/lib.rs
git commit -m "feat: add setup corrective guidance"
```

---

### Task 6: Update README and docs to the real setup flow

**Files:**
- Modify: `README.md`
- Modify: `docs/slack-setup.md`
- Modify: `docs/manual-smoke-test.md`

- [ ] **Step 1: Write the failing doc checklist**

```md
- [ ] README install flow starts with `cargo run -p rcc -- setup`
- [ ] Slack setup doc explains the interactive wizard and Slack bot onboarding links
- [ ] Manual smoke test assumes setup has already produced `.env.local` and mapping baseline
```

- [ ] **Step 2: Verify the current docs fail the checklist**

Run: `python - <<'PY'
from pathlib import Path
readme = Path('README.md').read_text()
setup = Path('docs/slack-setup.md').read_text()
smoke = Path('docs/manual-smoke-test.md').read_text()
checks = {
    'readme setup command': 'cargo run -p rcc -- setup' in readme,
    'slack setup wizard wording': 'Create app from manifest' in setup and 'cargo run -p rcc -- setup' in setup,
    'smoke assumes setup baseline': 'setup' in smoke.lower(),
}
for name, ok in checks.items():
    print(name, ok)
raise SystemExit(0)
PY`
Expected: at least one important `False` signal before editing.

- [ ] **Step 3: Rewrite the README install path**

Replace the Quickstart command block with:

```md
```bash
cargo run -p rcc -- setup
cargo run -p rcc -- doctor
cargo run -p rcc
```
```

And add one sentence that setup includes Slack bot onboarding.

- [ ] **Step 4: Rewrite `docs/slack-setup.md` around the real wizard**

Add a concise flow like:

```md
## 가장 짧은 흐름

```bash
cargo run -p rcc -- setup
```

`setup`이 아래를 순서대로 진행합니다.
- Slack app 생성 링크 안내
- manifest 경로 안내
- 토큰 입력
- channel mapping 입력
- `.env.local` 작성
- `doctor` 실행
```
```

- [ ] **Step 5: Tighten the smoke test preconditions**

Make the smoke test assume setup already ran successfully.

```md
## Preconditions

1. `cargo run -p rcc -- setup`이 완료되었습니다.
2. `cargo run -p rcc -- doctor`가 `[OK]` 상태입니다.
```

- [ ] **Step 6: Run doc verification**

Run: `python - <<'PY'
from pathlib import Path
required = {
    'README.md': ['cargo run -p rcc -- setup', 'cargo run -p rcc -- doctor', 'cargo run -p rcc'],
    'docs/slack-setup.md': ['Create app from manifest', 'cargo run -p rcc -- setup'],
    'docs/manual-smoke-test.md': ['cargo run -p rcc -- setup', 'cargo run -p rcc -- doctor'],
}
for path, needles in required.items():
    text = Path(path).read_text()
    missing = [needle for needle in needles if needle not in text]
    print(path, missing)
    if missing:
        raise SystemExit(1)
PY`
Expected: all files print empty missing lists.

- [ ] **Step 7: Commit**

```bash
git add README.md docs/slack-setup.md docs/manual-smoke-test.md
git commit -m "docs: document interactive setup flow"
```

---

### Task 7: Verify the full interactive setup feature

**Files:**
- Verify: `crates/app/src/main.rs`
- Verify: `crates/app/src/lib.rs`
- Verify: `crates/app/src/setup.rs`
- Verify: `README.md`
- Verify: `docs/slack-setup.md`
- Verify: `docs/manual-smoke-test.md`

- [ ] **Step 1: Run focused `rcc` tests**

Run: `cargo test -p rcc`
Expected: PASS, including the new setup tests.

- [ ] **Step 2: Run full workspace tests**

Run: `cargo test`
Expected: PASS across the workspace.

- [ ] **Step 3: Run setup in a temporary workspace smoke path**

Create a temporary copy of the required files and run the real setup command against it if the implementation supports controlled input for tests; otherwise rely on the automated setup tests plus a dry manual validation. The feature is not complete until one of those two verification paths is explicitly performed and reported.

- [ ] **Step 4: Run doctor after setup verification**

Run: `cargo run -p rcc -- doctor`
Expected: In a configured environment, all checks print `[OK]`. If your local environment is intentionally incomplete during development, report the actual `[FAIL]` lines rather than claiming success.

- [ ] **Step 5: Commit**

```bash
git add crates/app/src/main.rs crates/app/src/lib.rs crates/app/src/setup.rs README.md docs/slack-setup.md docs/manual-smoke-test.md
git commit -m "feat: add interactive setup wizard"
```

---

## Self-review

### Spec coverage
- `setup → doctor → run` command story is covered by Tasks 1 and 6.
- Slack bot onboarding links, manifest guidance, token capture, and project mapping are covered by Tasks 3 and 4.
- `.env.local` and `channel-projects.json` create/update behavior is covered by Task 3.
- Doctor reuse and corrective failure guidance are covered by Task 5.
- Required testing and docs updates are covered by Tasks 6 and 7.

### Placeholder scan
- No `TODO`/`TBD` placeholders remain.
- Every task includes exact file paths, commands, and concrete code snippets.
- The one manual verification branch in Task 7 explicitly states the acceptable verification paths instead of hand-waving.

### Type consistency
- The CLI command name is consistently `setup`.
- The package name is consistently `rcc`.
- The success path is consistently `setup → doctor → run`.
- Slack onboarding always references `slack/app-manifest.json` and “Create app from manifest”.
