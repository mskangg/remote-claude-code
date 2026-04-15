# Remote Claude Code setup hardening 설계

## 문서 목적

이 문서는 현재 1차 구현된 `rcc setup`를 실제로 “딸깍 설치”에 가까운 품질로 끌어올리기 위해, interactive 설치 UX와 non-interactive automation 경로를 함께 설계한다.

핵심 목표는 다음과 같다.

1. 사람은 계속 대화형 `setup`으로 설치할 수 있게 유지
2. Claude Code / smoke test / CI는 non-interactive 방식으로 설치를 끝낼 수 있게 만들기
3. 입력 방식이 달라도 최종 설치 엔진은 하나로 유지
4. hang 없이 실패하거나, 끝까지 완료되도록 설치 경로를 명확히 분기하기

---

## 현재 상태 요약

현재 저장소에는 다음이 이미 있다.

- `rcc setup` CLI 라우팅
- 대화형 prompt 기반 setup 흐름
- `.env.local` write/update
- `channel-projects.json` create/update
- 마지막 `doctor` 실행
- 관련 테스트 추가

하지만 실제 smoke 검증에서 다음 문제가 드러났다.

- interactive prompt를 외부에서 자동 입력으로 붙이기 어렵다
- 출력 형식이 automation에 충분히 우호적이지 않다
- 설치 흐름은 존재하지만, smoke/CI/Claude automation 관점에서는 hang 위험이 있다

즉 현재 상태는 **interactive setup 1차 구현**이지, 아직 완전한 딸깍 설치는 아니다.

---

## 문제 정의

이 프로젝트의 설치 UX는 두 부류를 모두 만족시켜야 한다.

### 1. 사람 사용자

- `cargo run -p rcc -- setup`
- 링크 보고 Slack app 생성
- 필요한 값 입력
- 끝나면 `doctor`
- 바로 `rcc` 실행

### 2. 자동화 주체

- Claude Code
- smoke test
- CI
- 향후 bootstrap script

이들은 prompt에 막히면 안 된다.

즉 installer는 단순히 친절한 대화형 CLI가 아니라,
**interactive frontend + automation frontend를 함께 가진 설치 엔진**이어야 한다.

---

## 목표 상태

최종적으로 지원해야 하는 대표 사용 방식은 아래 3개다.

### 1. 사람용 interactive

```bash
cargo run -p rcc -- setup
```

### 2. 자동화용 JSON 입력

```bash
cargo run -p rcc -- setup --from-file setup.json
```

### 3. 혼합형

```bash
RCC_SETUP_CHANNEL_ID=C123 cargo run -p rcc -- setup --from-file setup.json
```

즉 설치 입력은 여러 소스에서 오되, 최종 설치는 같은 엔진을 타야 한다.

---

## 권장 아키텍처

### 핵심 원칙

설치 실행 로직과 입력 수집 로직을 분리한다.

- 입력 수집 = 여러 frontend
- 설치 실행 = 하나의 core engine

이 구조가 필요한 이유는 다음과 같다.

1. interactive와 automation 경로를 동시에 지원할 수 있다
2. 테스트가 쉬워진다
3. hang 문제를 입력 단계에서 차단할 수 있다
4. writer / doctor / corrective action 로직을 중복하지 않게 된다

---

## 설치 데이터 모델

단일 설치 입력 모델을 둔다.

예시 개념:

- `slack_bot_token`
- `slack_signing_secret`
- `slack_app_token`
- `slack_allowed_user_id`
- `channel_id`
- `project_root`
- `project_label`

이 모델은 설치 엔진이 이해하는 **유일한 입력 포맷**이어야 한다.

중요한 점은, 모든 입력 소스가 결국 이 구조체를 채운 뒤 같은 writer/doctor 흐름으로 들어간다는 것이다.

---

## 입력 소스 우선순위

권장 우선순위는 아래와 같다.

1. CLI flag
2. `--from-file <json>`
3. env override
4. interactive prompt

즉 이미 채워진 값은 다시 묻지 않는다.

### 의미

- JSON이 기본 baseline 역할
- env는 override 역할
- interactive는 마지막 fallback 역할

이 순서가 좋은 이유는 다음과 같다.

- 사람이 실행하면 자연스럽게 prompt로 이어짐
- Claude는 file/env로 대부분 채울 수 있음
- smoke/CI는 prompt 진입 전에 fail fast 시킬 수 있음

---

## 동작 모드

### 모드 A. Interactive mode

호출:

```bash
cargo run -p rcc -- setup
```

특징:

- 부족한 값만 질문
- 링크 안내 포함
- Slack bot onboarding 포함
- 마지막 `doctor`

이 모드는 사람용 기본 UX다.

### 모드 B. Non-interactive file mode

호출:

```bash
cargo run -p rcc -- setup --from-file setup.json
```

특징:

- prompt 없이 입력 채움
- 부족한 값이 있으면 즉시 실패
- 실패 시 missing field 목록 출력

이 모드는 smoke/CI/Claude automation에 적합하다.

### 모드 C. Hybrid mode

호출:

```bash
RCC_SETUP_CHANNEL_ID=C123 cargo run -p rcc -- setup --from-file setup.json
```

특징:

- JSON baseline
- env override
- interactive fallback 가능

이 모드는 Claude Code가 설치를 대신할 때 특히 유용하다.

---

## Non-interactive 규칙

여기서 가장 중요한 규칙은 다음이다.

### automation 경로에서는 hang하면 안 된다

즉 non-interactive 상황에서는:

- prompt를 띄우는 대신
- 어떤 값이 부족한지 출력하고
- 즉시 종료해야 한다

예시 인상:

- missing: `SLACK_APP_TOKEN`
- missing: `channelId`
- fill these via `--from-file` or `RCC_SETUP_*`

이 규칙이 없으면 smoke/CI에서 installer는 신뢰할 수 없다.

---

## Interactive 규칙

interactive는 계속 제품처럼 느껴져야 한다.

원칙:

- 링크 먼저
- 설명은 짧게
- 이미 있는 값은 재사용 가능하게
- secret 재출력 금지
- 단계별 진행이 명확해야 함

즉 사람 기준으로는 여전히 “딸깍 설치처럼 느껴지는” UX를 유지해야 한다.

---

## JSON 입력 포맷

`--from-file`의 포맷은 설치 데이터 모델과 1:1로 대응해야 한다.

즉 JSON 파일은 다음 정보를 담는다.

- Slack token 4종
- `channelId`
- `projectRoot`
- `projectLabel`

원칙:

- key 이름은 `.env.local` 및 mapping schema와 최대한 자연스럽게 대응
- parser는 명확한 validation error를 제공
- invalid JSON / missing field / wrong type은 fail fast

---

## env override 규칙

env는 override로만 사용한다.

즉:

- JSON보다 우선
- interactive prompt보다 우선
- 설치 엔진 입력 모델을 채우는 마지막 override layer

이렇게 하면 Claude Code나 CI가 파일을 직접 수정하지 않고도 값을 주입할 수 있다.

예:

- `RCC_SETUP_SLACK_BOT_TOKEN`
- `RCC_SETUP_SLACK_SIGNING_SECRET`
- `RCC_SETUP_SLACK_APP_TOKEN`
- `RCC_SETUP_SLACK_ALLOWED_USER_ID`
- `RCC_SETUP_CHANNEL_ID`
- `RCC_SETUP_PROJECT_ROOT`
- `RCC_SETUP_PROJECT_LABEL`

---

## 내부 구조 권장

설치 로직은 아래 3개 층으로 나누는 것이 좋다.

### 1. Input resolution layer

역할:

- CLI flag
- JSON file
- env
- interactive prompt

이들을 합쳐 최종 `SetupInput`을 만든다.

### 2. Install execution layer

역할:

- `.env.local` write/update
- `channel-projects.json` write/update
- validation
- doctor 호출

즉 실제 설치 side effect는 여기서만 일어난다.

### 3. Reporting layer

역할:

- interactive 안내 메시지
- non-interactive missing field 출력
- `doctor` corrective action 출력

이 층을 분리하면 테스트와 UX 제어가 쉬워진다.

---

## smoke-friendly 요구사항

설치 기능이 진짜 완성되려면 아래가 가능해야 한다.

### smoke 검증 예시

```bash
cargo run -p rcc -- setup --from-file setup.json
cargo run -p rcc -- doctor
```

즉 smoke test는:

- prompt 없이
- deterministic 하게
- setup 완료 여부를 검증할 수 있어야 한다

이게 가능해져야 “딸깍 설치”를 실제로 반복 검증할 수 있다.

---

## Claude Code 친화성

이 구조는 Claude Code에도 매우 잘 맞는다.

Claude는 다음 중 하나를 선택할 수 있다.

### 방법 1
- interactive setup을 따라가며 입력

### 방법 2
- JSON 파일 생성
- `--from-file`로 setup 실행

### 방법 3
- env override로 민감값 주입
- setup 실행

즉 Claude는 더 이상 fragile한 prompt-parsing 자동화에 의존하지 않아도 된다.

---

## 실패 처리 원칙

### interactive 실패

- 어떤 단계에서 실패했는지 보여준다
- 다음 행동을 설명한다
- 가능하면 다시 입력받는다

### non-interactive 실패

- missing field 목록 출력
- invalid field 목록 출력
- prompt 진입 없이 종료

### doctor 실패

- 기존 corrective action 유지
- setup은 성공한 게 아니라 “설정은 썼지만 아직 준비 안 됨” 상태로 보고
- 다음 액션을 출력한다

---

## README 영향

이 기능이 보강되면 README의 설치 약속이 훨씬 더 진짜가 된다.

현재도:

```bash
cargo run -p rcc -- setup
cargo run -p rcc -- doctor
cargo run -p rcc
```

를 말하고 있지만, 이 구조가 완성되면 이 서사가 단순 문구가 아니라 실제로:

- 사람에게도
- Claude에게도
- smoke/CI에게도

유효한 온보딩 경로가 된다.

---

## 테스트 전략

최소 기준은 다음과 같다.

1. JSON 입력 parse 테스트
2. env override precedence 테스트
3. interactive fallback 테스트
4. non-interactive missing field fail-fast 테스트
5. install engine이 공통 로직을 타는지 테스트
6. `doctor`까지 연결되는지 테스트
7. 관련 crate test
8. 최종 `cargo test`

특히 중요한 회귀 테스트는:

- `--from-file`만으로 prompt 없이 완료되는지
- JSON + env override가 기대한 우선순위로 적용되는지
- missing field가 있을 때 hang하지 않고 종료되는지
- interactive mode에서는 기존 UX가 유지되는지

---

## 최종 권장 방향

setup hardening의 핵심은 단순히 prompt를 고치는 것이 아니다.

핵심은:

- **interactive installer**를 유지하면서
- **non-interactive installer**를 추가하고
- 둘 다 **같은 설치 엔진**을 타게 만드는 것이다

이 구조가 완성되면 Remote Claude Code의 설치 UX는 다음과 같이 말할 수 있다.

- 사람에게는 딸깍 설치처럼 느껴진다
- Claude Code가 대신 설치하기 쉽다
- smoke/CI가 hang 없이 검증 가능하다
- `doctor`로 끝까지 신뢰를 준다

즉 이 단계는 단순한 편의성 개선이 아니라,
**“딸깍 설치”를 실제로 증명 가능한 제품 경험으로 바꾸는 hardening 작업**이다.
