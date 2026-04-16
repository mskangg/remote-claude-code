---
name: remote-claude-code-setup-wizard
description: Use when setting up Remote Claude Code, connecting Slack, creating the Slack app from a manifest or manifest API, collecting setup values one at a time, resuming setup from the workspace artifact, running doctor verification, or guiding the user through this repository's installation wizard.
---

# Remote Claude Code Setup Wizard

## Overview
이 스킬은 Remote Claude Code 설치를 **plugin 기반 semi-automatic ping-pong wizard**로 진행한다. Claude가 가능한 건 자동 처리하고, Slack 콘솔처럼 사람 손이 필요한 단계는 정확한 링크, 붙여넣을 내용, 메뉴 위치, 복사할 값, 다음 행동을 한 단계씩 안내한다.

## When to Use
- 사용자가 `remote-claude-code 셋업해줘`, `슬랙 연동 설치해줘`, `딸깍 셋업 진행해줘` 같이 설치를 요청할 때
- setup을 처음 시작할 때
- setup을 중간에서 이어서 진행할 때
- artifact 기반으로 재개하거나 `doctor`까지 검증할 때

## Core Pattern

### 1. Start with local checks
먼저 아래를 확인한다.
- `tmux`
- `claude`
- workspace writable
- `slack/app-manifest.json`
- 기존 `.env.local`
- 기존 `data/channel-projects.json`
- 기존 `.local/slack-setup-artifact.json`

### 2. Resolve existing values first
먼저 아래 입력 소스를 재사용한다.
- 기존 `.env.local`
- 기존 channel mapping
- `--from-file`
- `--from-slack-artifact`
- `RCC_SETUP_*`

### 3. Treat app configuration token as the top branching point
Slack 앱 자동 생성을 가장 먼저 판단하는 기준은 **app configuration token**이다.
- 있으면: `apps.manifest.create`로 앱 생성 자동 시도
- 없으면: token 발급 단계를 안내
- 발급도 못 하면: 검증된 수동 manifest 경로로 fallback

config token 발급 안내:
- Slack 앱 관리 화면으로 이동
- `Generate Token`
- 원하는 workspace 선택
- 생성된 token 복사
- 그 값을 한 번만 받기

### 4. Use manual Slack console flow as the stable fallback
- 링크: `https://api.slack.com/apps?new_app=1`
- `Create app from manifest`
- workspace 선택
- `slack/app-manifest.json` 내용 붙여넣기
- 완료 후 `계속`

### 5. Collect values one at a time
순서:
1. `Signing Secret` — `Basic Information` → `Signing Secret`
2. `Bot User OAuth Token` — `OAuth & Permissions` → 설치 후 확인
3. `App-Level Token` — `connections:write` 권한으로 만든 `xapp-...`
4. `allowedUserId` — 프로필 → 세 점 → `Copy member ID`

### 6. Use the artifact bridge for machine steps
기본 artifact 경로:
- `.local/slack-setup-artifact.json`

유용한 명령:
```bash
cargo run -p rcc -- setup --write-slack-artifact-template .local/slack-setup-artifact.json
cargo run -p rcc -- setup --merge-slack-artifact <patch.json> --json
cargo run -p rcc -- setup --from-slack-artifact .local/slack-setup-artifact.json --non-interactive
cargo run -p rcc -- doctor
cargo build --release -p rcc
./target/release/rcc
```

### 7. Always check readiness before resume
- `ready: false`면 `missing` 필드만 다시 요청
- `ready: true`면 바로 resume

## Final message
Slack 콘솔 단계만 잠깐 따라오면 Claude가 나머지 설치를 끝까지 마무리하고 실행 가능한 바이너리까지 준비해 준다.
