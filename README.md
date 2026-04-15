# Remote Claude Code

![GitHub stars](https://img.shields.io/github/stars/mskangg/remote-claude-code?style=flat&color=yellow)
![License](https://img.shields.io/badge/license-MIT-green)
![Slack First](https://img.shields.io/badge/transport-Slack--first-4A154B)
![Rust](https://img.shields.io/badge/built%20with-Rust-orange)

> 지금 작업 중인 Claude Code를 Slack에서 그대로 이어서 부릴 수 있습니다.

새 에이전트도, 새 원격 IDE도 필요 없습니다. 지금 일하던 로컬/클라우드 작업환경 그대로, 심지어 휴대폰에서도 같은 Claude Code 세션에 일을 시킬 수 있습니다.

[Quickstart](#quickstart) · [Doctor](#doctor) · [How it works](#how-it-works) · [Roadmap](#roadmap)

![Remote Claude Code hero demo](docs/images/hero-demo.gif)

- **기존 Claude Code 세션을 어디서든 이어서 사용**
- **새 에이전트나 새 원격 개발환경을 강요하지 않음**
- **설치 후 `doctor`로 바로 검증 가능**

![Remote Claude Code vibe shot](docs/images/hero-view.jpg)

## Why this is different

### 이건 이런 제품이 아닙니다

- 새로운 agent platform
- 별도의 remote IDE
- 지금 작업환경을 버리고 옮겨 타는 시스템

### 이건 이런 제품입니다

- **Slack이 원격 UI가 됩니다**
- **Claude Code는 원래 작업하던 환경에서 계속 실행됩니다**
- **당신은 같은 세션을 어디서든 이어서 부립니다**

핵심은 새로운 환경을 배우는 게 아니라, **원래 쓰던 Claude Code를 더 멀리까지 가져가는 것**입니다.

## Quickstart

```bash
cargo run -p rcc -- setup
cargo run -p rcc -- doctor
cargo run -p rcc
```

`setup`은 Slack bot onboarding 링크 안내, manifest 경로 안내, 토큰 입력, channel mapping 작성, 그리고 마지막 `doctor` 검증까지 순서대로 진행합니다.

앱 실행 뒤 Slack에서 `/cc`를 실행하면 됩니다.

더 자세한 설정은 [`docs/slack-setup.md`](docs/slack-setup.md)에서 볼 수 있습니다.

## Doctor

`doctor`는 “지금 바로 되는 상태인가?”를 빠르게 확인하기 위한 명령입니다.

현재 다음 항목을 검증합니다.

- Slack 토큰 4종
- `.env.local` 존재 여부
- `tmux` 사용 가능 여부
- 상태 DB 경로 생성 가능 여부
- hook events 디렉터리 생성 가능 여부
- `slack/app-manifest.json` 존재 여부
- `data/channel-projects.json` 존재 여부

앱 실행 전에 아래 명령부터 돌리면 됩니다.

```bash
cargo run -p rcc -- doctor
```

## How it works

- Slack은 첫 번째 원격 UI입니다.
- Claude Code는 기존 로컬 또는 클라우드 작업환경에서 계속 실행됩니다.
- tmux, session, hook relay를 통해 상태와 최종 응답이 Slack thread로 돌아옵니다.
- 앞으로는 같은 모델을 Discord, Telegram까지 확장할 수 있습니다.

## Use cases

### Away from desk
자리에서 벗어나도 휴대폰으로 같은 Claude Code 세션에 작업을 이어서 시킬 수 있습니다.

### In transit
이동 중에도 코드 리뷰, 파일 검토, 다음 액션 정리 같은 일을 Slack thread로 지시할 수 있습니다.

### Long-running sessions
긴 작업을 하나의 thread/session 흐름으로 유지하면서 상태와 최종 응답을 계속 추적할 수 있습니다.

## Setup and docs

- Slack 설정: [`docs/slack-setup.md`](docs/slack-setup.md)
- 수동 점검: [`docs/manual-smoke-test.md`](docs/manual-smoke-test.md)
- Hero export: [`docs/hero-export.md`](docs/hero-export.md)
- 런치 카피 팩: [`docs/launch-copy.ko.md`](docs/launch-copy.ko.md)

## Roadmap

- Slack-first public launch
- easier setup and onboarding
- Discord transport
- Telegram transport

## Current limitations

- 현재 공개 대상은 Slack 기준으로 설계되어 있습니다.
- `rcc setup slack`은 아직 구현되지 않았습니다.
- 지금은 `.env.local`과 Slack 앱 생성이 필요합니다.
- 런타임/운영 안정성은 계속 강화 중입니다.
