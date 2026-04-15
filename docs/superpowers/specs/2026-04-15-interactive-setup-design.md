# Remote Claude Code 대화형 설치 설계

## 문서 목적

이 문서는 Remote Claude Code에 `setup` 기반의 대화형 설치 흐름을 추가해, 사용자가 문서를 길게 읽지 않고도 실행 가능한 상태까지 빠르게 도달할 수 있도록 하는 방향을 정의한다.

핵심 목표는 다음과 같다.

1. 사용자가 `rcc setup`만으로 설치를 진행할 수 있게 만들기
2. Slack app 생성, 토큰 입력, 프로젝트 매핑, 검증까지 터미널 안에서 순차적으로 안내하기
3. 마지막 검증은 기존 `doctor`를 재사용해 신뢰를 유지하기
4. 이 흐름을 Claude Code가 대신 수행해도 자연스럽게 만들기

---

## 문제 정의

현재 설치 흐름은 다음 성격을 가진다.

- README와 `docs/slack-setup.md`를 읽고
- Slack app manifest를 수동으로 적용하고
- `.env.local`을 손으로 작성하고
- `data/channel-projects.json`을 손으로 만들고
- `doctor`를 실행한 뒤
- 앱을 수동 실행한다

이 방식은 동작은 가능하지만, public 오픈소스 관점에서는 진입 마찰이 크다.

특히 이 프로젝트의 타겟 사용자는 Claude Code 사용자이므로, 다음 기준이 중요하다.

- 딸깍 설치에 가까운 경험
- 터미널 안에서 단계별 안내
- 필요한 링크를 그때그때 제공
- 실패 시 무엇을 해야 하는지 바로 알 수 있음
- Claude Code에게 “설치해줘”라고 맡기기 쉬운 구조

---

## 목표 상태

사용자는 저장소를 받은 뒤 아래 흐름만 기억하면 된다.

```bash
cargo run -p rcc -- setup
cargo run -p rcc -- doctor
cargo run -p rcc
```

실제 UX는 `setup`이 대부분을 담당한다.

즉 사용자는:

1. `setup` 실행
2. 터미널의 링크/질문을 따라 Slack bot/app 생성
3. 토큰 입력
4. project mapping 입력
5. `doctor` 성공 확인
6. 바로 `rcc` 실행 가능 상태 도달

여기까지 installer의 책임이다.

---

## 범위

### installer가 책임지는 것

- prerequisites의 핵심 항목 점검
- Slack manifest 경로 안내
- Slack app 생성 링크 안내
- Slack bot onboarding을 설치 흐름 안에 포함
- 토큰 4종 입력 받기
- `projectRoot`, `projectLabel`, `channelId` 입력 받기
- `.env.local` 작성
- `data/channel-projects.json` 생성 또는 갱신
- `doctor` 실행
- 성공/실패에 따라 다음 액션 안내

### installer가 책임지지 않는 것

- Slack에서 `/cc`까지 실제 성공 검증
- full smoke test 수행
- 메시지 relay 전체 검증
- 런타임 상태 문제까지 자동 복구

즉 installer는 **실행 가능한 상태를 만드는 것**까지만 책임지고, first-run success는 별도 검증 단계로 둔다.

---

## 권장 아키텍처

### 접근 방식

`setup`과 `doctor`를 분리하되, installer의 마지막 단계에서 `doctor`를 반드시 호출하는 하이브리드 구조를 사용한다.

- `setup` = 설치 진행과 입력 수집
- `doctor` = 최종 검증과 상태 보고

이 구조의 장점은 다음과 같다.

1. 설치 UX는 wizard로 매끄럽게 만들 수 있다
2. 검증 로직은 기존 자산을 재사용할 수 있다
3. README 상에서도 메시지가 선명하다
   - `setup`
   - `doctor`
   - `run`
4. Claude Code가 setup을 대신 수행할 때도 단계가 명확하다

---

## CLI 표면

### 새 명령

```bash
cargo run -p rcc -- setup
```

향후 binary 사용 시:

```bash
rcc setup
```

### 기존 명령 유지

```bash
cargo run -p rcc -- doctor
cargo run -p rcc
```

즉 설치 서사는 아래 3개 명령으로 고정된다.

1. `setup`
2. `doctor`
3. `run`

---

## 설치 wizard UX

### 단계 1. 소개

첫 화면에서 짧게 설명한다.

- 이 설치가 무엇을 준비하는지
- Slack-first 설치라는 점
- 마지막에 `doctor`로 검증한다는 점

출력은 짧고 screenshot-friendly 해야 한다.

예시 톤:

- “Remote Claude Code Slack-first setup을 시작합니다.”
- “몇 가지 값을 입력하면 실행 가능한 상태까지 준비합니다.”

### 단계 2. prerequisites 확인

우선 아래 항목을 확인한다.

- `tmux`
- `claude`
- 워크스페이스 루트 접근 가능 여부
- manifest 파일 존재 여부

이 단계는 hard fail / soft fail을 나눠야 한다.

#### hard fail
- `tmux` 없음
- manifest 없음
- 쓰기 불가능한 디렉터리

#### soft fail / 안내
- 아직 `.env.local` 없음
- 아직 `data/channel-projects.json` 없음

즉 setup이 앞으로 만들 수 있는 것은 실패가 아니라 정상적인 초기 상태로 본다.

---

## Slack app 생성 단계

이 단계의 핵심은 사용자가 Slack 설정 페이지에서 헤매지 않게 만들고, Slack bot onboarding 자체를 installer 흐름 안에 포함시키는 것이다.

### wizard가 해야 할 일

- `slack/app-manifest.json` 경로를 명확히 보여준다
- Slack app 생성 방식이 “Create app from manifest”임을 설명한다
- 해당 페이지로 가야 하는 링크를 출력한다
- 사용자가 app 생성을 마친 뒤 다음으로 넘어가게 한다

### UX 원칙

- 긴 설명 금지
- 링크 + manifest 경로 + 다음 행동만 제시
- “생성 완료했으면 Enter” 같은 진행 방식 허용

이 단계는 **설치 네비게이터**처럼 동작해야 한다.

---

## 토큰 입력 단계

`setup`은 아래 4개 값을 입력받는다.

- `SLACK_BOT_TOKEN`
- `SLACK_SIGNING_SECRET`
- `SLACK_APP_TOKEN`
- `SLACK_ALLOWED_USER_ID`

### 각 입력에서 제공할 것

- 값 이름
- 어디서 찾는지 짧은 설명
- 필요하면 관련 링크

### 보안 원칙

- 이미 입력한 secret을 다시 출력하지 않는다
- 입력 실패 시 값 전체를 로그에 남기지 않는다
- `.env.local` 작성 시 필요한 형태만 저장한다

### UX 원칙

- secret 입력은 단계별로 분리
- 각 단계는 짧아야 함
- “이 값은 어디서 찾나요?”를 최소화하도록 문구를 제공

---

## 프로젝트 매핑 단계

Slack app 설정 후, 세션 실행에 필요한 로컬 매핑을 수집한다.

필수 입력:

- `channelId`
- `projectRoot`
- `projectLabel`

### wizard가 해야 할 일

- `projectRoot`가 실제 디렉터리인지 확인
- `channelId`와 `projectLabel`은 저장 형식에 맞춰 반영
- `data/channel-projects.json`이 없으면 생성
- 이미 있으면 append/update 정책을 가져야 함

### UX 원칙

- 처음 사용자 기준으로 설명
- 하지만 장황하면 안 됨
- `projectRoot`는 절대경로 입력을 유도

---

## 파일 쓰기 단계

### `.env.local`

`setup`은 수집한 값을 `.env.local`에 쓴다.

원칙:

- 워크스페이스 루트의 `.env.local`만 사용
- 상위 fallback 금지
- 기존 값이 있을 경우 overwrite 정책을 명확히 해야 함

권장 정책:

- 기존 파일이 없으면 생성
- 기존 파일이 있으면 해당 key만 update
- 다른 key는 유지

### `data/channel-projects.json`

원칙:

- 파일이 없으면 생성
- 있으면 JSON 구조를 유지하며 append/update
- 예제 파일 구조와 동일한 포맷 유지

---

## 검증 단계

installer의 마지막 단계는 반드시 `doctor`다.

### 동작 방식

- 내부적으로 `doctor`를 호출
- 결과를 그대로 보여주되, setup UX에 맞게 요약 가능
- all ok면 success
- fail이 있으면 다음 corrective action 안내

### 성공 시 출력

최소한 아래를 알려줘야 한다.

- `.env.local` 준비 완료
- channel mapping 준비 완료
- `doctor` 성공
- 이제 `cargo run -p rcc` 실행 가능

### 실패 시 출력

단순히 `[FAIL]`만 보여주고 끝내면 안 된다.

필수:

- 어떤 항목이 실패했는지
- 사용자가 다음에 뭘 해야 하는지
- 필요하면 관련 문서 경로

즉 stack trace보다 corrective action이 먼저여야 한다.

---

## Claude Code와의 궁합

이 설치 흐름은 사람이 직접 따라가도 되고, Claude Code가 대신 진행해도 자연스러워야 한다.

### 이를 위해 필요한 성질

- 단계가 선형적일 것
- 입력 요구가 명확할 것
- 링크와 파일 경로가 출력에 포함될 것
- 마지막 성공 조건이 `doctor`로 명확할 것

즉 사용자는 이렇게 말할 수 있어야 한다.

```text
이 repo 설치해줘. setup 흐름대로 진행하고 doctor까지 성공시켜줘.
```

그리고 Claude Code는 wizard의 출력만 읽어도 다음 행동을 이어갈 수 있어야 한다.

---

## README/문서 영향

이 기능이 생기면 README 설치 흐름도 단순해진다.

### 현재

- manifest 설명
- `.env.local` 설명
- mapping 설명
- `doctor`
- 실행

### 변경 후

- `cargo run -p rcc -- setup`
- `cargo run -p rcc -- doctor`
- `cargo run -p rcc`

즉 public launch 관점에서도 onboarding 메시지가 훨씬 강해진다.

---

## 실패 처리 원칙

### hard stop

아래는 installer가 중단해야 한다.

- 필수 파일이 없음
- 디렉터리 쓰기 불가
- `tmux` 없음
- 입력 형식이 명백히 잘못됨

### recoverable

아래는 안내 후 다시 입력받을 수 있다.

- Slack token 누락
- project path 오입력
- channel id 오입력
- `doctor`의 일부 fail

즉 setup은 최대한 “다음 액션을 제시하는 설치 안내자”처럼 동작해야 한다.

---

## 테스트 전략

이 기능은 설치 기능이므로 문서만으로는 부족하고, 테스트가 중요하다.

최소 기준:

1. setup 입력 흐름 테스트
2. `.env.local` write/update 테스트
3. `channel-projects.json` create/update 테스트
4. setup 마지막에 doctor를 호출하는지 테스트
5. doctor fail 시 corrective action 출력 테스트
6. 관련 crate test
7. 최종 `cargo test`

특히 회귀 방지를 위해 다음을 자동화해야 한다.

- 기존 env key를 덮어쓸 때 다른 key가 보존되는지
- mapping file이 깨지지 않는지
- 빈 설치 상태에서 정상적으로 첫 setup이 가능한지

---

## 최종 권장 방향

이 프로젝트의 설치 UX는 아래처럼 기억되어야 한다.

```bash
cargo run -p rcc -- setup
cargo run -p rcc -- doctor
cargo run -p rcc
```

그리고 사용자가 느껴야 하는 인상은 다음과 같다.

- 문서를 길게 읽지 않아도 된다
- 터미널이 다음 행동을 알려준다
- Slack app 생성 링크와 manifest 경로를 바로 준다
- 토큰 입력 후 자동으로 로컬 설정이 된다
- 마지막에 `doctor`로 신뢰를 준다
- 바로 실행 가능한 상태까지 간다

즉 이 installer는 단순한 설정 명령이 아니라,
**Remote Claude Code의 public onboarding을 제품처럼 느끼게 만드는 핵심 UX 구성요소**다.
