# Slack Setup

## 목표

설치는 딸깍이거나, 최소한 Claude Code에게 맡길 수 있을 정도로 단순해야 합니다.

Remote Claude Code의 첫 공개는 Slack-first입니다. 지금은 사용자가 자신의 Slack workspace에 앱을 만들고 `.env.local`을 채운 뒤 `doctor`로 검증하는 흐름을 가장 짧게 만드는 데 집중합니다.

## 가장 짧은 흐름

```bash
cargo run -p rcc -- setup
```

`setup`이 아래를 순서대로 진행합니다.

- Slack app 생성 링크 안내 (`Create app from manifest`)
- `slack/app-manifest.json` 경로 안내
- 토큰 4종 입력
- channel mapping 입력
- `.env.local` 작성
- `doctor` 실행

그 다음 실행 흐름은 아래처럼 고정됩니다.

```bash
cargo run -p rcc -- doctor
cargo run -p rcc
```

## Claude Code에게 맡길 때

이 프로젝트의 사용자는 Claude Code에 익숙하다는 전제를 둡니다. 완전한 딸깍 설치가 아니더라도, 아래처럼 Claude Code에게 setup을 맡길 수 있어야 합니다.

```text
이 저장소의 Slack 설정을 진행해줘. `slack/app-manifest.json` 경로를 써서 앱 생성 단계를 안내하고, `.env.local`에 필요한 값을 채우고, 마지막에 `cargo run -p rcc -- doctor`까지 실행해줘.
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
