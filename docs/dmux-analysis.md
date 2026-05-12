# dmux 정독 노트 — Rust swarm 오케스트레이터 설계용

대상: `~/codes/dmux` (formkit/standardagents, v5.7.1, TS + Ink).
목적: tmux + 멀티 agent + worktree 기반 협업 오케스트레이터(가칭 `conclave`)를 Rust로 처음부터 짤 때, dmux가 풀어둔 알고리즘·패턴을 정확히 차용하기 위함.

본 노트는 임시 위치(`~/codes/`)에 두고, 새 레포 부트스트랩 시 `docs/dmux-analysis.md`로 이동.

---

## 1. 아키텍처 한 줄

```
TUI (Ink/React)
   ↓
StatusDetector ─── coordinator (EventEmitter)
   ├── per-pane PaneWorker  (worker_thread)  ← 실제 settle 알고리즘
   │      ↓ analysis-needed
   └── PaneAnalyzer            (LLM judge via OpenRouter)
   
TmuxService (singleton)        ← tmux CLI 래퍼
TmuxHookManager                ← SIGUSR2 기반 tmux→app IPC
WorktreeCleanupService         ← 직렬 큐, 백그라운드 정리
```

핵심 분리:
- **"무엇이 화면에서 일어나고 있나"**(저비용 폴링) — PaneWorker
- **"이 화면이 어떤 의미인가"**(LLM 호출) — PaneAnalyzer
- 두 단계는 **debounced & cached**되어 LLM 호출 빈도가 매우 낮음

---

## 2. Settle Detection — 가장 중요한 알고리즘

`src/workers/PaneWorker.ts`

### 2.1 상수
```ts
CAPTURE_LINE_COUNT       = 50    // 매 polling마다 캡처할 줄 수
USER_TYPING_SETTLE_MS    = 3500  // 사용자 타이핑 후 대기
AGENT_ACTIVITY_SETTLE_MS = 1500  // agent 활동 후 대기
pollIntervalMs           = 1000  // 1Hz polling
```

### 2.2 상태머신
```
working ──static·1.5s+·새 콘텐츠──> analyzing ──LLM──> idle | waiting
   ↑                                                       │
   └───────────── activity 재개 ──────────────────────────┘

user typing detected → revert to prev status, hold for 3.5s
```

### 2.3 핵심 데이터
- `captureHistory: Array<{raw, fingerprint}>` — 롤링 윈도우 5
- `settledStateConfirmed: bool` — 한 번 idle 판정되면 활동 재개 전까지 LLM 재요청 금지 (스팸 방지)
- `lastStaticFingerprint` — "이미 LLM에 보낸 스냅샷"의 핑거프린트
- `lastUserInteractionAt`, `lastAgentActivityAt` — debounce 타임스탬프

### 2.4 매 tick 알고리즘
```
1. capture last 50 lines
2. build fingerprint
3. (agent==codex && sentinel file changed) → idle, return  ← out-of-band 신호 우선
4. last 20 lines에 "agent working indicator" 있나? (esc to interrupt, *ing... 등)
     yes → mark working, reset history
5. fingerprint를 history에 push (max 5)
6. history < 3 → return (판정 보류)
7. 모든 fingerprint 동일? → static
       no (다양함):
         prev vs curr가 "user typing 패턴"인가?
           yes → user-interaction event, hold 3.5s
           no  → mark agent active
       yes (정적):
         < USER_TYPING_SETTLE_MS since user → wait
         < AGENT_ACTIVITY_SETTLE_MS since agent → wait
         awaiting agent after user typing → wait
         fingerprint != lastStaticFingerprint && !settledStateConfirmed
            && ≥5s since last analysis
         → transitionToAnalyzing(content)   ← LLM 발사
```

### 2.5 우리(conclave)에서의 적응

dmux는 **agent CLI 종류가 11개라 일반화**되어 있어 LLM judge가 필수. 우리는 **Claude만 상대**한다는 단순화가 있다 + **agent CLI 자체에 종료 hook이 있다** (Claude `Stop` hook). 따라서:

**1차 신호 (정확)**: Claude `Stop` hook이 sentinel 파일을 touch
- `.swarm/<session>/<pane>.idle` 또는 JSON 이벤트 파일
- dmux의 codex hook 패턴과 동일 (`worktree/.codex/dmux/<paneId>.json`)
- Rust 측은 `notify` crate로 inotify/fsevents 감시
- **이게 있으면 polling 알고리즘 전체를 우회**

**2차 신호 (fallback)**: dmux의 screen-watch 알고리즘 그대로
- Claude 특유 패턴 정확히 알므로 LLM judge 안 써도 됨:
  - `(esc to interrupt)` 화면에 있음 → working
  - 마지막 비공백 줄이 `>` 시작 + 위 패턴 없음 → idle
  - `[A]ccept`, `[Y]es/[N]o` 류 → waiting (regex로 충분)
- 50줄·1.5s·5s 임계값은 dmux 그대로 시작, 측정 후 튜닝

**3차 신호 (보험)**: LLM judge (옵션)
- 1·2 모두 모호할 때만 발사
- CEO가 이미 Claude니까 CEO 자신에게 위임 (`claude -p`)
- 별도 OpenRouter 종속 제거

### 2.6 user typing 감지 알고리즘 (`isLikelyUserTyping`)
`src/utils/paneAttentionHeuristics.ts:205`

```
prev/curr에서 "trailing prompt block" 추출 (>, $, ❯, › 시작 줄 + 그 이후 연속 라인)
prev_prefix == curr_prefix && prev_prompt != curr_prompt → 타이핑
변경 줄이 1~6개 이내 + 모두 화면 하단 6줄 안 + 공통 prefix 70%+ + prompt-like → 타이핑
```

협업 swarm에서는 사용자가 agent pane을 거의 안 건드리는 경우가 많지만, 디버깅 시 사람이 끼어들 수도 있으니 차용 가치 있음.

---

## 3. PaneAnalyzer (LLM judge) — 우리는 대부분 안 쓸 것

`src/services/PaneAnalyzer.ts`

### 3.1 3단 LLM 파이프라인
```
Stage 1: determineState(content) → 'option_dialog' | 'open_prompt' | 'in_progress'
Stage 2: if option_dialog → extractOptions(content) → {question, options[], potentialHarm, attentionTitle, attentionBody}
         if open_prompt    → extractSummary(content) → {summary, attentionTitle, attentionBody}
         else              → return state
```

### 3.2 모델 race
```ts
modelStack = ['google/gemini-2.5-flash', 'x-ai/grok-4-fast:free', 'openai/gpt-4o-mini']
Promise.any(modelStack.map(tryModel))   // 병렬 race, 첫 성공 채택
```
순차 fallback(6s+)을 병렬 race(~1s)로 바꾼 게 핵심 최적화. 우리에겐 비해당(Claude만 씀).

### 3.3 캐시·dedup
- MD5(content) 키, 5s TTL, max 100 entries LRU
- 동일 `${paneId}:${hash}` 진행 중인 promise는 reuse

### 3.4 정규화
```ts
function normalizePaneContentForAnalysis(content, maxLines=50):
  trim 주변 공백 줄 → last 50줄 → 다시 trim → join
```
LLM에 보내기 전 모든 stage가 같은 50줄을 보도록 보장.

### 3.5 LLM 프롬프트에서 빌려올 휴리스틱 (가장 가치 큰 부분)
**dmux Stage 1 system prompt에서 추출한 Claude 특이 패턴들:**

| 패턴 | 의미 |
|---|---|
| `"(esc to interrupt)"` 어디서든 | 무조건 `in_progress` |
| spinner glyph (✶ ⏺ ✽ ⏳ 🔄) + `*ing...` 단어 | `in_progress` |
| `Pondering...`, `Crunching...`, `Flibbergibberating...` | Claude Code 특유 작업 단어 |
| `⏵⏵ accept edits on` (esc-to-interrupt 없이) | `open_prompt` (정적 UI) |
| 빈 `>` 프롬프트 | `open_prompt` |
| `[y/n]`, `1) ... 2) ...`, `[A]ccept` | `option_dialog` |

이 패턴들은 **Rust regex 정적 검사**로 LLM 없이 처리 가능.

---

## 4. TmuxService — Rust 매핑

`src/services/TmuxService.ts` (1431 LOC, 싱글톤)

### 4.1 retry 전략 분리
```ts
enum RetryStrategy { IDEMPOTENT, FAST, NONE }
```
- **read 쿼리** → IDEMPOTENT (재시도 OK)
- **write 작업** (split/resize/send-keys) → FAST (1~2회 재시도, 빠른 backoff)
- **permanent error**(`can't find pane` 등) → 재시도 안 함

Rust에서는 `backoff` crate or 단순 loop.

### 4.2 가장 중요한 API surface
| 메서드 | tmux 명령 |
|---|---|
| `splitPane({cwd, command})` → new paneId | `tmux split-window -h -P -F '#{pane_id}' -c 'cwd' 'command'` |
| `sendShellCommand(paneId, cmd)` | quote-escape 후 `send-keys` |
| `sendTmuxKeys(paneId, "Enter")` | raw `send-keys` (no quote) |
| `getPaneContent(paneId, {start, end})` | `capture-pane -p` (history range) |
| `paneExists(paneId)` | `list-panes -F '#{pane_id}'` membership |
| `getAllPaneInfo()` | 1회의 `list-panes -F` batched query |
| `killPane(paneId)` | `kill-pane -t` (errors swallowed if missing) |
| `setOption(opt, val)` | `set-option -g` |
| `refreshClient()` | `refresh-client` |

### 4.3 **footgun: sendShellCommand vs sendTmuxKeys**
```ts
// 잘못된 패턴
sendKeys(paneId, "git commit -m 'fix bug'")   // tmux가 공백에서 깨짐

// 올바른 패턴
sendShellCommand(paneId, "git commit -m 'fix bug'")   // auto-quote
sendTmuxKeys(paneId, "Enter")                          // key sequence
```
Rust 매핑: `send_shell()` vs `send_keys()` 두 API로 분리. 합치면 안 됨.

### 4.4 batched query 패턴
```
tmux display-message -p "#{a}|#{b}|#{c}|#{d}|#{e}"
```
4번 호출할 거 1번에 끝냄. `|` 구분자 split. 우리도 차용.

### 4.5 spawn 시 새 paneId 받기
```
tmux split-window -h -P -F '#{pane_id}' [-c cwd] [command]
```
`-P -F` 콤보 없으면 새 pane ID 모름 → 추가 list-panes 필요. **반드시 콤보 사용**.

---

## 5. WorktreeCleanupService

`src/services/WorktreeCleanupService.ts` (237 LOC)

### 5.1 직렬 큐
```ts
cleanupQueue: Promise<void> = Promise.resolve()
enqueueCleanup(job) {
  cleanupQueue = cleanupQueue.then(() => runCleanup(job)).catch(log)
}
```
체이닝으로 1개씩 순차 처리. 큰 worktree 삭제가 UI를 막지 않음.

Rust: `tokio::sync::mpsc` + 단일 consumer task.

### 5.2 nested worktree 처리
- `detectAllWorktrees()`로 트리 탐색 → depth desc 정렬 → 깊은 것부터 `git worktree remove --force`
- 같은 worktree에 nested worktree가 있을 수 있다는 사실 자체가 놓치기 쉬움

### 5.3 branch 삭제 정책
```
deleteBranch=true → 각 nested repo에서 git show-ref --verify 후 git branch -D
```
존재 확인 후에만 삭제. silent 실패 허용.

### 5.4 hook trigger 타이밍
worktree remove 시도 후 **성공/실패 무관**하게 `triggerHook('worktree_removed')` 발사. 정리 실패해도 후속 단계는 진행.

### 5.5 우리(conclave) 적용
swarm 단계별로:
- **회의 단계** — worktree 없음, read-only
- **합의 후 분담 단계** — agent별 worktree 생성 (dmux 패턴 그대로)
- **머지/포기 단계** — cleanup queue로 백그라운드 정리

---

## 6. TmuxHookManager — SIGUSR2 IPC

`src/services/TmuxHookManager.ts` (285 LOC)

### 6.1 핵심 아이디어
tmux는 임의 콜백을 지원하지 않지만 `run-shell`을 실행할 수 있고, 그 안에서 `kill -USR2 $PID`로 우리 프로세스에 신호 전달.

```bash
tmux set-hook -t '$SESSION' after-split-window \
  'run-shell "kill -USR2 ${OUR_PID} 2>/dev/null || true # dmux-hook"'
```

마커 `# dmux-hook`으로 자기가 설치한 hook 식별 (uninstall 시 사용).

### 6.2 hooks 4종
- `after-split-window` → pane-created
- `pane-exited` → pane-closed
- `client-resized` → pane-resized
- `after-select-pane` → pane-focus-changed

### 6.3 동작
1. `process.on('SIGUSR2', ...)`로 신호 listener 등록
2. SIGUSR2 들어오면 `'hook-triggered'` 이벤트 emit (debounced 100ms)
3. listener는 tmux를 다시 쿼리해 "뭐가 바뀌었는지" 확인

### 6.4 Rust 매핑
```rust
use tokio::signal::unix::{signal, SignalKind};
let mut sig = signal(SignalKind::user_defined2())?;
loop {
    sig.recv().await;
    // debounce, then query tmux
}
```

대안: hook이 sentinel 파일을 touch하고 `notify` crate로 감시. PID 추적 안 해도 됨, 동시 여러 인스턴스도 가능. **추천**.

### 6.5 fallback
hook 설치 권한 없거나 거부 시 → `panePollingWorker.ts` (5s `tmux list-panes` 폴링). 우리도 두 모드 유지.

---

## 7. Agent working indicator 디테일

`src/utils/paneAttentionHeuristics.ts`

### 7.1 정의
```ts
GENERIC_PROGRESS_WORDS = [
  "working", "thinking", "planning", "pondering", "crunching",
  "analyzing", "building", "testing", "running", "searching",
  "reviewing", "understanding", "loading", "processing",
  "writing", "reading", "editing", "patching", "generating",
  "reasoning", "compiling", "indexing", "summarizing",
  "executing", "refactoring", "fixing", "checking", "scanning",
]

SPINNER_PREFIX = '[⠁-⣿◐◓◑◒◴◷◶◵●○◦•·⋯⋮✦✧✶✻✽⏳⌛]'   // braille + dots + sparkles + hourglass
```

### 7.2 매치 규칙 (`hasAgentWorkingIndicators`)
```
recent_relevant_lines (마지막 10줄, trim+filter empty)
  /esc\s+to\s+(interrupt|cancel|stop|abort)/i              → true
  ^SPINNER + (progress_word)(\b|...|…|\s)                  → true
  progress_word\b.*(\.\.\.|…|\d+%|/\d+)                    → true
  (claude만) "claude is working" + "germinating|thinking..." → true
```

### 7.3 우리에게 시사
Claude만 다룰 거면 regex 3개로 충분:
```rust
static ESC_INTERRUPT: Lazy<Regex> = Lazy::new(|| Regex::new(r"esc\s+to\s+(interrupt|cancel|stop|abort)").unwrap());
static SPINNER_WORK:   Lazy<Regex> = Lazy::new(|| Regex::new(r"^[⠁-⣿◐◓◑◒◴◷◶◵●○]\s*\w+ing").unwrap());
static PROGRESS_PCT:   Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\w+ing\b.*(\.\.\.|\d+%)").unwrap());
```

---

## 8. Rust 의존성 짧은 리스트

dmux 분석 후 conclave에서 실제로 필요한 의존성:

| 용도 | crate |
|---|---|
| async runtime | `tokio` (full features) |
| 프로세스 spawn (claude CLI) | `tokio::process::Command` |
| 파일 watching (sentinel) | `notify` v6 |
| signal handling | `tokio::signal::unix` (libc) |
| regex (working indicators) | `regex` + `once_cell` |
| serde | `serde` + `serde_json` |
| CLI | `clap` v4 |
| error | `anyhow` (앱 레벨) + `thiserror` (라이브러리 경계) |
| logging | `tracing` + `tracing-subscriber` |
| UUID | `uuid` v1 |
| sha256 (fingerprint) | `sha2` 또는 단순 `xxhash-rust` |

**필요 없는 것** (vs dmux):
- ANSI 파서 (TerminalDiffer 대응) — 우리는 화면 스트리밍 안 함, fingerprint만 필요
- React/Ink UI — v0 미정, ratatui는 v1 이후
- HTTP client (OpenRouter) — Notion/kodex MCP는 CEO Claude가 처리
- 11개 agent 어댑터 — Claude만

---

## 9. 우리에게 가장 중요한 차용 우선순위

| 우선순위 | 차용 대상 | 형태 |
|---|---|---|
| ★★★ | PaneWorker settle 알고리즘 (50줄·1.5s·5s·rolling 5) | Rust로 직번역 |
| ★★★ | sendShellCommand vs sendTmuxKeys 분리 | 두 함수로 명확히 |
| ★★★ | split-window + `-P -F` 로 새 pane ID 받기 | helper 1줄 |
| ★★★ | working indicator regex (Claude 패턴) | regex 3개 + lazy_static |
| ★★ | WorktreeCleanup 직렬 큐 + depth-desc 정렬 | tokio mpsc consumer |
| ★★ | SIGUSR2/sentinel file IPC for tmux hooks | notify crate가 더 깔끔 |
| ★★ | batched display-message 쿼리 | `\|` 구분자 split |
| ★ | retry 전략 분리 (read=idempotent, write=fast) | backoff crate or 단순 loop |
| ★ | content-hash + LRU cache (LLM 결과 캐시용, 옵션) | `lru` crate |
| 미차용 | TerminalDiffer (ANSI 풀 파서) | 안 필요 |
| 미차용 | PaneAnalyzer 3단 LLM 파이프라인 | sentinel + regex로 대체 |
| 미차용 | Promise.any 모델 race | 단일 모델(Claude) |

---

## 10. 결정점 & 가정

본 분석을 토대로 design.md에서 확정해야 할 항목:

1. **Sentinel signal protocol** — Claude `Stop` hook에서 어떤 JSON 포맷의 파일을 어디에 쓸지
2. **회의 단계 vs 실행 단계 분리** — 회의에서는 worktree 없음, 실행에서만 worktree per agent
3. **CEO의 위치** — (a) 별도 tmux pane의 claude 프로세스 vs (b) conclave 바이너리 위에서 돌리는 claude `-p` 호출 루프
4. **Notion·kodex 호출 주체** — CEO Claude (MCP 직접 호출). conclave 바이너리는 절대 호출 안 함
5. **이름·레포 경로** — `conclave`? `~/codes/conclave`?

이 5개는 다음 단계(Phase 2 design.md)에서 user와 확정.
