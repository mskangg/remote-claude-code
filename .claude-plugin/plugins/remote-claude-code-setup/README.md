# remote-claude-code-setup

Guided setup wizard plugin for Remote Claude Code.

## What it provides

- Verified semi-automatic Slack setup as the default path
- Step-by-step collection of required Slack values
- Slack manifest delivery via GitHub blob link, raw link, or inline JSON
- Artifact-based setup resume flow
- `doctor` verification
- Final release binary guidance
- Experimental manifest API path kept as a non-default option

## Best path

1. Let the wizard guide you through the Slack console steps
2. Paste the manifest from the provided link or raw URL
3. Provide the requested values one at a time
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
