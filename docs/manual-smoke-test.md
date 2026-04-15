# Manual Smoke Test

Slack-first 공개 전에 실제로 첫 성공 경험이 되는지 확인하는 체크리스트입니다.

## Preconditions

1. `cargo run -p rcc -- setup`이 완료되었습니다.
2. `cargo run -p rcc -- doctor`가 `[OK]` 상태입니다.
3. 매핑된 `projectRoot`는 실제 로컬 디렉터리입니다.
4. `tmux`와 `claude`가 `PATH`에서 실행 가능합니다.
5. public channel이면 manifest 변경 후 Slack app을 다시 설치했고, private channel이면 bot을 미리 초대했습니다.

## Doctor

먼저 아래 명령이 모두 `[OK]`를 출력해야 합니다.

```bash
cargo run -p rcc -- doctor
```

Expected:
- every line prints `[OK]`

하나라도 `[FAIL]`이 나오면 Slack 실행 전에 먼저 고칩니다.

## Slack Run

`doctor`가 통과한 뒤에만 앱을 실행합니다.

```bash
cargo run -p rcc
```

Expected:
- process stays up
- no immediate startup error

## Session Start

매핑된 Slack channel에서:

1. `/cc` 실행
2. channel에 root message가 생기는지 확인
3. 새 thread 안에 status message가 생기는지 확인

Expected:
- `.local/state.db` 아래에 SQLite state file 생성
- `.local/hooks/` 아래에 hook file 생성
- 새로운 tmux session 생성

Useful checks:

```bash
tmux ls
ls .local/hooks
```

## Thread Reply

thread 안에서 짧은 프롬프트를 보냅니다.

```text
say hello and stop
```

Expected:
- status message가 같은 thread에 유지됨
- Claude가 매핑된 project directory에서 실행됨
- Claude가 끝나면 status가 정리됨
- 최종 assistant reply가 thread에 게시됨

## Failure Path

실패할 만한 프롬프트를 보내거나 수동 interrupt를 시도합니다.

Expected:
- runtime failure면 failure 상태가 보임
- thread에 failure message가 게시됨
- 프로세스 전체는 계속 살아 있어야 함

## Concurrency Check

1. 서로 다른 mapped channel에서 두 세션 시작
2. 거의 동시에 프롬프트 전송

Expected:
- separate tmux sessions
- separate hook files
- no cross-posting between threads
- final replies return to the correct owning thread
