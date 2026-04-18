# CLAUDE.md

## 목적 & 핵심 기준

Slack 안에서 Claude Code 세션을 안정적으로 원격 제어하는 제품을 출시한다.

- 타입스크립트 버전과 동일하거나 더 나은 사용자 경험을 제공해야 한다.
- 구조 개선은 허용된다. 다만 구조 개선이 실사용 회귀를 만들면 실패다.
- "동작하는 코드"보다 "운영 가능한 코드"를 우선한다.
- TS보다 후진 UX가 나오면 그 변경은 미완성이다.

## 아키텍처

크레이트 계층 구조:

| 크레이트 | 역할 |
|---|---|
| `crates/app` | bootstrap, wiring, env/config (조립만 한다) |
| `crates/application` | 유스케이스, 오케스트레이션, Slack 제품 동작 규칙 |
| `crates/transport-slack` | Slack Socket Mode ingress, API adapter, payload 파싱 |
| `crates/runtime-local` | tmux 실행, Claude 프로세스 실행, hook file polling |
| `crates/session-store` | SQLite 영속화 |
| `crates/core-service` | session actor / reducer / runtime forwarding 정책 |
| `crates/core-model` | 도메인 식별자, 상태, 메시지 모델 |

의존성 규칙:

- `application`은 `transport-slack`의 adapter 인터페이스를 사용해도 된다.
- `transport-slack`는 유스케이스를 직접 소유하지 않는다.
- `runtime-local`과 `session-store`는 인프라다. 제품 정책을 넣지 않는다.

환경:

- Rust는 워크트리 루트의 `.env.local`만 본다.
- 기본 상태 DB: `.local/state.db` / 기본 hook event 디렉터리: `.local/hooks`
- 언어: `RCC_LOCALE=ko` or `RCC_LOCALE=en` (기본 `en`)

## 개발 / 빌드

- 바이너리 크레이트 이름은 `rcc`: `cargo test -p rcc`, `cargo build -p rcc` (`app`으로 하면 오류)
- 전체 테스트: `cargo test --workspace`
- 린트: `cargo clippy --all-targets --all-features` (경고 0개 기대)
- 로그: `RUST_LOG=debug rcc` (기본 INFO; tracing_subscriber 설치됨)
- 새 크레이트 추가 시 각 `Cargo.toml`에 `[lints]\nworkspace = true` 필수

## 제품 UX 규칙

### /cc 메뉴

- `/cc`는 바로 세션을 만들지 않는다. 메인 메뉴(`새 세션 열기` / `기존 세션 보기`)를 먼저 보여준다.

### 기존 세션 보기

- 텍스트만 보여주면 안 된다. 세션 목록 block UI를 사용한다.
- 각 항목: 프로젝트명, tmux session name, thread ts, `스레드 열기` 버튼

### 세션 thread

- thread 안에서는 slash command에 의존하지 않는다.
- 세션 제어 진입점은 thread 안의 `명령어` 버튼 (command palette)
- palette 최소 액션: `Interrupt` / `Esc` / `Clear` / `CLAUDE.md update` / `세션 종료`

### 세션 종료

- `세션 종료`는 tmux session 종료를 의미한다.
- 종료 후 stale action이 눌려도 프로세스는 죽으면 안 된다.

### 상태 메시지

- `Working...` 상태 메시지는 thread root를 편집하는 방식이 아니다.
- 각 turn마다 **별도의 status message를 새로 만든다** → turn 중 갱신 → turn 완료 시 삭제.
- 최종 답변은 새 thread message로 올린다.
- 금지: root message edit / 완료 답변을 status message edit로 대체 / 삭제된 status 재사용
- 기본 상태: `작업 중...` 계열. hook progress event가 있으면 구체적으로 갱신.

## 런타임 규칙

### Hook

- Claude 종료/응답 relay는 hook file 기준으로 처리한다. tmux pane 상태만 보고 판단하지 않는다.
- hook `Stop` / `StopFailure`가 최종 전달 기준이다.
- hook progress event(`PreToolUse`, `PostToolUse`)는 status message 갱신에 사용한다.
- turn은 순차적으로 관리 가능한 구조여야 한다. 이전 turn 완료 전에 다음 입력이 들어와도 매핑이 꼬이지 않도록.
- terminal event가 와도 pending turn이 없으면 프로세스가 죽으면 안 된다.
- **초기화 순서:** `configure_slack_lifecycle_observer`는 반드시 `recover_active_sessions` **이전**에 호출. 순서가 바뀌면 recovery 중 reply가 유실된다.
- hook poller: 연속 5회 실패 → `RuntimeFailed` emit 후 종료 (`MAX_CONSECUTIVE_FAILURES = 5`)

### WebSocket

- WebSocket open 실패: 최대 10회 재시도 후 fatal (`MAX_CONSECUTIVE_OPEN_FAILURES = 10`)

### tmux

- 앱 시작 시 orphan UUID tmux session 정리를 수행한다.
- DB에 존재하지 않는 UUID 세션만 정리. 사용자가 직접 쓰는 `slack-*` 등 일반 세션은 건드리지 않는다.

### 에러 처리

non-fatal (로그 + continue):

- 종료된 세션에 대한 stale action
- 없는 status message update 실패, permalink 조회 실패, 개별 Slack action 실패
- transcript recovery에서 `HOME` 미설정 → non-fatal skip (에러로 처리하면 poller 실패 카운터에 쌓임)

fatal (프로세스 종료):

- 프로세스 부팅 실패
- Slack Socket Mode 연결 자체 실패
- 필수 env/config 누락

## 작업 기준

우선순위 (항상 이 순서):

1. 사용자 체감 회귀 수정
2. 데이터/세션 일관성
3. 프로세스 생존성
4. TS parity
5. 구조 개선 (단, 위 1~4를 더 안정적으로 만드는 경우 즉시 진행 가능)

테스트 기준:

- 새 기능/회귀 수정은 반드시 테스트를 먼저 또는 함께 추가한다.
- 필수 회귀 대상: `/cc` 메뉴, 새 세션 생성, 기존 세션 보기, thread reply relay, status message 생성/갱신/삭제, 최종 답변 relay, command palette, `세션 종료` 후 stale action, orphan tmux cleanup

구현 태도:

- 구조 욕심은 허용된다. 단 출시가 목표이므로 구조는 제품 안정성을 높이는 방향으로만 바꾼다.
- "예쁜 구조"보다 "운영 중 회귀를 줄이는 구조"를 택한다.

## 하네스

이 저장소의 session lifecycle, Slack UX, hook relay, stale action 작업은 `remote-claude-harness` 스킬로 처리한다. 단순 질문은 직접 응답 가능.
