# CLAUDE.md

## 목적

Slack 안에서 Claude Code 세션을 안정적으로 원격 제어하는 제품을 출시한다.

- "동작하는 코드"보다 "운영 가능한 코드"를 우선한다.
- 구조 개선이 실사용 회귀를 만들면 실패다. TS보다 후진 UX가 나오면 그 변경은 미완성이다.

## 아키텍처

`app` → `application` → `transport-slack` / `runtime-local` / `session-store` → `core-service` → `core-model`

- `application`: 유스케이스 + Slack 제품 동작 규칙 (제품 정책의 집결지)
- `transport-slack`: Slack Socket Mode + API adapter (유스케이스 소유 금지)
- `runtime-local`: tmux + hook file polling (인프라, 제품 정책 금지)
- `session-store`: SQLite 영속화 (인프라)
- `app`: 조립만 한다

의존성 규칙:

- `application`은 `transport-slack`의 adapter 인터페이스를 사용해도 된다.
- `transport-slack`는 유스케이스를 직접 소유하지 않는다.
- `runtime-local`과 `session-store`는 인프라다. 제품 정책을 넣지 않는다.
- `app`은 조립만 한다. 비즈니스 로직을 추가하지 않는다.

## 개발 / 빌드

- 바이너리 크레이트 이름은 `rcc`: `cargo test -p rcc`, `cargo build -p rcc` (`app`으로 하면 오류)
- 전체 테스트: `cargo test --workspace` / 린트: `cargo clippy --all-targets --all-features`
- 로그: `RUST_LOG=debug rcc` (기본 INFO; tracing_subscriber 설치됨)
- 언어: `RCC_LOCALE=ko|en` / 환경 파일: `.env.local` (워크트리 루트)
- 새 크레이트 추가 시 `Cargo.toml`에 `[lints]\nworkspace = true` 필수

## 제품 UX 규칙

- `/cc` → 메인 메뉴 먼저 (`새 세션 열기` / `기존 세션 보기`). 바로 세션 만들지 않는다.
- 세션 목록은 block UI를 사용한다 (텍스트 전용 금지).
- thread 안에서는 slash command 대신 `명령어` 버튼으로 제어한다.
- `세션 종료` = tmux session 종료. 종료 후 stale action이 와도 프로세스는 죽으면 안 된다.

**상태 메시지 (중요):**
- turn마다 **별도의 status message 신규 생성** → turn 중 갱신 → turn 완료 시 삭제.
- 최종 답변은 새 thread message. 금지: root message edit / 완료 답변을 status edit로 대체.

## 런타임 규칙

**초기화 순서 (필수):** `configure_slack_lifecycle_observer`는 `recover_active_sessions` **이전**에 호출.
순서가 바뀌면 recovery 중 runtime event의 Slack reply가 유실된다.

- hook `Stop`/`StopFailure`가 최종 전달 기준. tmux pane 상태로 "아마 끝났음" 판단 금지.
- hook poller: 연속 5회 실패 → `RuntimeFailed` emit 후 종료 (`MAX_CONSECUTIVE_FAILURES = 5`)
- WebSocket open 실패: 최대 10회 재시도 후 fatal (`MAX_CONSECUTIVE_OPEN_FAILURES = 10`)
- orphan tmux 정리: UUID 세션 중 DB에 없는 것만. 일반 세션(`slack-*` 등) 건드리지 않음.
- transcript recovery에서 `HOME` 미설정 → non-fatal skip (에러 처리하면 poller 실패 카운터에 쌓임)

non-fatal: stale action, status update 실패, permalink 실패, 개별 Slack action 실패  
fatal: 부팅 실패, Socket Mode 연결 실패, 필수 env/config 누락

## 작업 기준

우선순위: **사용자 체감 회귀** > 데이터 일관성 > 프로세스 생존성 > TS parity > 구조 개선

- 새 기능/회귀 수정은 테스트를 먼저 또는 함께 추가한다.
- 회귀 테스트 필수: `/cc` 메뉴, 세션 생성/목록, thread relay, status message lifecycle, command palette, 세션 종료 후 stale action, orphan tmux cleanup

## 하네스

session lifecycle, Slack UX, hook relay, stale action 작업이면 `remote-claude-harness` 스킬을 사용한다.
