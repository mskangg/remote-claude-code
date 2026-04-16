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

config token 발급 안내는 이렇게 한다.
- Slack 앱 관리 화면으로 이동
- `Generate Token`
- 원하는 workspace 선택
- 생성된 token 복사
- 그 값을 사용자에게 한 번만 받기

### 4. Use manual Slack console flow as the primary stable fallback
브라우저 전면 자동화는 기본 경로가 아니다. 실제로 검증된 성공 경로는 아래였다.

- 링크: `https://api.slack.com/apps?new_app=1`
- `Create app from manifest`
- workspace 선택
- `slack/app-manifest.json` 내용 붙여넣기
- 완료 후 `계속`

manifest가 필요하면 아래 중 하나를 제공한다.
- GitHub 링크
- raw 링크
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

### Step 3. Config token check
> 먼저 Slack 앱을 자동으로 만들 수 있는지 확인할게요.
> 그러려면 **Slack app configuration token**이 필요합니다.
> 이 토큰이 있으면 제가 Slack 앱 생성을 먼저 자동으로 시도할 수 있어요.
> 이미 있으면 붙여주세요. 없으면 지금 발급 단계로 안내할게요.

### Step 3-B. Config token issuance
> 좋아요. 그럼 Slack app configuration token부터 만들게요.
> 이 토큰은 **Slack 앱을 자동 생성할 때 쓰는 전용 토큰**입니다.
>
> 1. Slack 앱 관리 화면으로 이동
> 2. `Generate Token` 클릭
> 3. 사용할 workspace 선택
> 4. 생성된 token 복사
> 5. 그 값을 저에게 붙여넣어 주세요

설명 보강:
> `Generate Token`을 누른 뒤 바로 끝나는 게 아니라, 원하는 workspace를 선택한 다음 토큰을 만들어야 합니다.
> 이 토큰이 있으면 앱 생성 단계 일부를 제가 대신 처리할 수 있습니다.

### Step 4. Manifest API create attempt
성공 시:
> Slack 앱 생성은 자동으로 끝났어요. 이제 설치 승인과 토큰 회수만 하면 됩니다.

실패 시:
> Slack 앱 자동 생성은 실패했어요. 이제 검증된 수동 manifest 생성 단계로 진행할게요.

### Step 5. Manual manifest step
> 지금은 Slack 콘솔 단계예요.
> 1. 아래 링크를 열어 주세요.
> 2. `Create app from manifest`를 선택해 주세요.
> 3. workspace를 선택해 주세요.
> 4. 아래 manifest를 붙여넣어 앱을 생성해 주세요.
> 완료되면 `계속`이라고 보내주세요.

### Step 6. Signing Secret
> 먼저 `Signing Secret`를 보내주세요.
> 위치: `Basic Information` → `Signing Secret`

### Step 7. Bot token
> 좋아요. 다음은 `Bot User OAuth Token`입니다.
> 형식: `xoxb-...`
> 위치: `OAuth & Permissions` → 설치 후 확인

### Step 8. App-level token
> 다음은 `App-Level Token`입니다.
> `connections:write` 권한으로 만든 `xapp-...` 값을 보내주세요.

### Step 9. Allowed user ID
> 마지막으로 `allowedUserId`가 필요합니다.
> Slack에서 `프로필 → 세 점 → Copy member ID`로 확인한 `U...` 값을 보내주세요.

### Step 10. Artifact readiness
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
