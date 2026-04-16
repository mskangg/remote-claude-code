# Slack Setup

## 목표

설치는 딸깍이거나, 최소한 Claude Code에게 맡길 수 있을 정도로 단순해야 합니다.

Remote Claude Code의 첫 공개는 Slack-first입니다. 지금은 사용자가 자신의 Slack workspace에 앱을 만들고 `.env.local`을 채운 뒤 `doctor`로 검증하는 흐름을 가장 짧게 만드는 데 집중합니다.

## 가장 짧은 흐름

```bash
cargo run -p rcc -- setup --slack-config-token <xoxa-config-token>
```

`setup`은 automation-first entrypoint입니다.

기본 흐름:
- 기존 값 / file / env override 먼저 확인
- app configuration token이 있으면 `apps.manifest.create`를 먼저 시도
- 성공하면 설치 승인/토큰 회수 단계만 남김
- 실패하거나 token이 없으면 검증된 manual-assisted Slack 단계로 fallback
- 생성 결과를 setup 입력/artifact로 흡수
- `.env.local` 작성
- channel mapping 작성
- `doctor` 실행
- release binary 실행 경로 안내

그 다음 실행 흐름은 아래처럼 고정됩니다.

```bash
cargo run -p rcc -- doctor
cargo build --release -p rcc
./target/release/rcc
```

## Automation-friendly setup

### API-first path

```bash
cargo run -p rcc -- setup --slack-config-token <xoxa-config-token>
```

Slack app configuration token이 있으면 setup은 `apps.manifest.create`를 먼저 시도합니다.
성공하면 artifact에 `signingSecret`, `appId`, `oauthAuthorizeUrl`를 반영하고, 이후에는 설치 승인과 토큰 회수 단계만 진행하면 됩니다.

#### app configuration token이 없을 때

먼저 **Slack app configuration token**을 발급합니다.
이 토큰은 Slack 앱을 자동 생성할 때 쓰는 전용 토큰입니다.

1. Slack 앱 관리 화면으로 이동
2. `Generate Token` 클릭
3. 사용할 workspace 선택
4. 생성된 app configuration token 복사
5. 그 값을 setup에 넣기

즉 `Generate Token`을 누른 뒤 바로 끝나는 게 아니라, 원하는 workspace를 선택한 다음 token을 만들어야 합니다.
이 토큰이 있으면 setup이 `apps.manifest.create`를 먼저 시도할 수 있습니다.

### Fallback semi-automatic path

Slack artifact 기본 경로:

- `.local/slack-setup-artifact.json`

manual-assisted Slack 단계에 들어가면 `setup`이 이 파일을 자동 생성합니다.
이미 알고 있는 값(file/env/기존 상태)은 자동 prefill되고, Slack 단계에서 새로 얻은 값만 채우면 됩니다.

Slack artifact patch 예시:

- `docs/slack-setup-artifact-patch.example.json`

브라우저 보조나 수동 단계가 새로 알아낸 값만 부분적으로 반영할 때는 아래처럼 patch를 merge합니다.
이 command는 patch를 merge한 직후 artifact가 재개 가능한 상태인지도 함께 출력합니다.

```bash
cargo run -p rcc -- setup --merge-slack-artifact docs/slack-setup-artifact-patch.example.json
```

Slack artifact를 다시 흡수하는 경로:

```bash
cargo run -p rcc -- setup --from-slack-artifact .local/slack-setup-artifact.json --non-interactive
```

Optional env overrides:
- `RCC_SETUP_SLACK_BOT_TOKEN`
- `RCC_SETUP_SLACK_SIGNING_SECRET`
- `RCC_SETUP_SLACK_APP_TOKEN`
- `RCC_SETUP_SLACK_ALLOWED_USER_ID`
- `RCC_SETUP_CHANNEL_ID`
- `RCC_SETUP_PROJECT_ROOT`
- `RCC_SETUP_PROJECT_LABEL`

non-interactive 규칙:
- 가능한 값은 file/env/기존 상태에서 채웁니다.
- 값이 부족하면 hang하지 않고 즉시 실패합니다.
- Slack 콘솔 개입이 필요한 단계는 manual-assisted 단계로 분리합니다.
- Slack 단계가 끝나면 artifact JSON을 다시 넣어 setup을 재개할 수 있습니다.

## Claude Code에게 맡길 때

이 프로젝트의 사용자는 Claude Code에 익숙하다는 전제를 둡니다. 완전한 무인 설치가 아니더라도, Claude가 아래 순서로 setup을 주도해야 합니다.

```text
이 저장소의 Slack 설정을 진행해줘. 먼저 app configuration token이 있는지 확인하고, 있으면 `apps.manifest.create`를 먼저 시도해줘. token이 없거나 API 생성이 실패하면 `slack/app-manifest.json` 기준으로 검증된 manual-assisted Slack 단계를 진행해줘. Slack 단계에서 새로 얻은 값은 `docs/slack-setup-artifact-patch.example.json` 형태의 patch JSON으로 정리해서 `cargo run -p rcc -- setup --merge-slack-artifact <patch.json> --json`로 반영해줘. 마지막에는 `cargo run -p rcc -- setup --from-slack-artifact .local/slack-setup-artifact.json --non-interactive`, `cargo run -p rcc -- doctor`, `cargo build --release -p rcc`, `./target/release/rcc`까지 이어서 진행해줘.
```

## 필요한 값

`.env.local`에는 최소 다음 항목이 필요합니다.

- `SLACK_BOT_TOKEN`
- `SLACK_SIGNING_SECRET`
- `SLACK_APP_TOKEN`
- `SLACK_ALLOWED_USER_ID`

## Manifest-first setup

번들된 manifest 경로:

`slack/app-manifest.json`

현재 manifest는 public channel에서 `/cc` 루트 메시지를 생성할 수 있도록 필요한 scope를 포함합니다. private channel에서는 테스트 전에 bot을 초대해야 합니다.

## Channel mapping

`data/channel-projects.example.json`을 복사해 `data/channel-projects.json`을 만들고, 아래 값을 실제 환경에 맞게 바꿉니다.

- `channelId`
- `projectRoot`
- `projectLabel`

## Doctor가 확인하는 것

현재 `doctor`는 다음 항목을 확인합니다.

1. required Slack env vars 존재 여부
2. `.env.local` 존재 여부
3. `tmux` 사용 가능 여부
4. 상태 DB 경로 생성 가능 여부
5. hook events 디렉터리 생성 가능 여부
6. `slack/app-manifest.json` 존재 여부
7. `data/channel-projects.json` 존재 여부

## UX rules

- secret 값은 터미널에 다시 출력하지 않습니다.
- setup 문구는 짧고 복붙 가능해야 합니다.
- 실패 시 stack trace보다 다음 corrective action을 먼저 보여줘야 합니다.
- 현재는 Slack-first지만, 이후 transport는 Discord, Telegram으로 확장할 수 있습니다.
