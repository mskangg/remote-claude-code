# remote-claude-code-setup

Guided setup wizard plugin for Remote Claude Code.

## What it provides

- API-first Slack app creation through `apps.manifest.create` when a **Slack app configuration token** is available
- Verified semi-automatic Slack fallback when API creation is unavailable or fails
- Step-by-step collection of required Slack values
- Artifact-based setup resume flow
- `doctor` verification
- Final release binary guidance

A Slack app configuration token is the token used to create the Slack app automatically. If the user does not have one yet, the setup wizard explains where to generate it and which workspace to choose before continuing.

## Best path

1. Generate a Slack app configuration token first
2. Let setup try manifest API creation first
3. If Slack still needs manual work, follow the guided Slack console steps
4. Resume from the workspace artifact
5. Finish with `doctor` and a release build

## Trigger phrases

- `remote-claude-code 셋업해줘`
- `슬랙 연동 설치해줘`
- `딸깍 셋업 진행해줘`
- `셋업 계속해줘`
- `설치 상태 점검해줘`

## Primary skill

- `remote-claude-code-setup-wizard`

This plugin ships the setup wizard skill inside the plugin package under `./skills/`, so installation does not depend on the repository root `.claude/skills` path.
