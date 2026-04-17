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

### 3. Use manual Slack console flow as the primary stable path
브라우저 전면 자동화나 manifest API-first 경로는 기본 경로가 아니다. 실제로 검증된 성공 경로는 아래였다.

- 링크: `https://api.slack.com/apps?new_app=1`
- `From a manifest` 선택
- workspace 선택
- `slack/app-manifest.json` 내용 붙여넣기
- 완료 후 `계속`

manifest가 필요하면 아래 중 하나를 제공한다.
- GitHub 보기용 링크: `https://github.com/mskangg/remote-claude-code/blob/main/slack/app-manifest.json`
- raw 링크: `https://raw.githubusercontent.com/mskangg/remote-claude-code/main/slack/app-manifest.json`
- inline JSON

### 5. Collect values one at a time
반드시 한 번에 하나씩 받는다.

순서:
1. `Signing Secret`
   - 위치: `Basic Information` → `Signing Secret`
2. `Bot User OAuth Token`
   - 형식: `xoxb-...`
   - 위치: `OAuth & Permissions` → 설치 후 확인
3. `App-Level Token`
   - 형식: `xapp-...`
   - `connections:write` 권한으로 생성
   - 위치: `Basic Information` → `App-Level Tokens`
4. `allowedUserId`
   - 형식: `U...`
   - 위치: 프로필 → 세 점 → `Copy member ID`
5. `projectRoot`
   - 형식: 절대 경로
   - 먼저 어떤 로컬 프로젝트를 Slack 채널과 연결할지 정한다
6. `projectLabel`
   - 형식: Slack에서 보일 프로젝트 이름
   - 기본적으로는 `projectRoot`의 basename을 후보로 본다
7. `channelId`
   - 형식: `C...`
   - 채널 하나가 프로젝트 하나를 대표한다는 점을 먼저 안내한다
   - 프로젝트용 채널을 만든 뒤 bot user를 초대한 다음 채널 세부정보에서 수집한다

### 6. Use the artifact bridge for all machine steps
기본 artifact 경로:
- `.local/slack-setup-artifact.json`

patch 예시:
- `docs/slack-setup-artifact-patch.example.json`

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
`--merge-slack-artifact --json` 결과를 보고 판단한다.
- `ready: false`면 `missing` 필드만 다시 요청한다.
- `ready: true`면 바로 `resumeCommand`를 실행한다.

### 8. Finish with doctor and release binary
설치가 끝나면 마지막은 개발용 `cargo run`이 아니라 릴리즈 빌드와 빌드 산출물 실행 경로 안내다.

```bash
cargo build --release -p rcc
./target/release/rcc
```

## Final Step Sequence

### Step 0. Start
> Remote Claude Code 셋업을 시작할게요. 자동으로 할 수 있는 건 처리하고, Slack 콘솔에서 필요한 단계만 짧고 정확하게 안내할게요.

### Step 1. Local checks
> 먼저 로컬 환경과 기존 설정을 확인할게요.

실패 시:
> 아직 바로 진행할 수 없어요. 먼저 아래를 해결해 주세요.
> - `tmux`
> - `claude`
> - manifest 파일
> - 쓰기 권한

### Step 2. Baseline resolution
> 이미 알고 있는 값은 먼저 채워둘게요.

### Step 3. Manual manifest step
> 지금은 Slack 콘솔 단계예요.
> 1. 아래 링크를 열어 주세요.
> 2. `From a manifest`를 선택해 주세요.
> 3. workspace를 선택해 주세요.
> 4. 아래 manifest를 붙여넣어 앱을 생성해 주세요.
> 완료되면 `계속`이라고 보내주세요.

### Step 6. Signing Secret
> 먼저 `Signing Secret`를 보내주세요.
> 위치: `Basic Information` → `Signing Secret`

### Step 7. Bot token
> 좋아요. 다음은 `Bot User OAuth Token`입니다.
> 먼저 `OAuth & Permissions`로 가서 **`Install to 'your-workspace'`** 를 누르세요.
> 설치가 끝나면 같은 화면의 **OAuth Tokens** 섹션에 `Bot User OAuth Token`(`xoxb-...`)이 생성됩니다.
> 그 값을 보내주세요.

### Step 8. App-level token
> 다음은 `App-Level Token`입니다.
> `connections:write` 권한으로 만든 `xapp-...` 값을 보내주세요.

### Step 9. Allowed user ID
> 다음은 `allowedUserId`입니다.
> Slack에서 `프로필 → 세 점 → Copy member ID`로 확인한 `U...` 값을 보내주세요.

### Step 10. Project root
> 이제 어떤 로컬 프로젝트를 연결할지 정할게요.
> `projectRoot` 절대 경로를 보내주세요.

### Step 11. Project label
> 이 프로젝트가 Slack에서 어떤 이름으로 보일지 정할게요.
> `projectLabel`을 보내주세요. 원하면 `projectRoot` basename 기준으로 정해도 됩니다.

### Step 12. Channel ID
> 이제 이 프로젝트를 대표할 Slack 채널을 준비할 차례예요. 채널 하나가 프로젝트 하나를 대표합니다.
> 이 프로젝트용 채널을 새로 만들거나 기존 채널을 고른 뒤, 반드시 **`/invite @Remote Claude Code`** 로 방금 만든 bot user를 먼저 초대해 주세요.
> 그 다음 채널 세부정보를 열고 맨 아래의 `Copy channel ID`를 눌러 `C...` 값을 보내주세요.
> 초대 전에는 `/cc` 루트 메시지는 보여도 thread reply가 세션으로 전달되지 않을 수 있습니다.

### Step 11. Artifact readiness
부족할 때:
> 아직 바로 재개할 수는 없어요.
> 부족한 값:
> - `...`

완료 시:
> 필요한 값이 모두 채워졌어요. 이제 setup을 이어서 진행할게요.

### Step 11. Resume setup
> artifact 기준으로 setup을 이어서 진행합니다.

### Step 12. Doctor verification
> 설치 검증을 진행할게요.

### Step 13. Release build handoff
> 설치와 검증이 끝났어요. 이제 실행 파일을 빌드합니다.
>
> ```bash
> cargo build --release -p rcc
> ./target/release/rcc
> ```

## Common mistakes
- 토큰 여러 개를 한 번에 달라고 하기
- 값 위치 설명 없이 `값만 주세요`라고 하기
- 브라우저 자동화를 주 경로로 가정하기
- config token 분기를 맨 앞에 두지 않기
- artifact readiness 확인 없이 바로 resume 하기
- 마지막 실행을 `cargo run -p rcc`로 안내하기

## Quick Reference
- 최우선 분기점: `app configuration token` 유무
- 안정적인 기본 경로: semi-automatic ping-pong flow
- member ID 확인: 프로필 → 세 점 → `Copy member ID`
- App-Level Token scope: `connections:write`
- 마지막 실행: `cargo build --release -p rcc` 후 `./target/release/rcc`
- 목표 메시지: Slack 콘솔 단계만 잠깐 따라오면 Claude가 나머지 설치를 끝까지 마무리해 줌
