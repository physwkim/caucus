# caucus — design.md

> Rust로 구현하는 **AI 에이전트 팀용 라이브 터미널 멀티플렉서**.
>
> **2026-05-16 피벗.** caucus는 "tmux 위의 비동기 회의 프로토콜"에서 "자체
> 라이브 멀티플렉서"로 목적을 바꿨다. 본 문서는 전체가 새 모델 기준이다 —
> 구 모델(tmux pane·파일 sentinel·비동기 라운드)의 설명은 더 이상 남아 있지 않다.

## 0. 결정 사항 (locked, 2026-05-16 피벗 반영)

| # | 결정 |
|---|---|
| 1 | 이름: `caucus`. 경로: `~/codes/caucus`. |
| 2 | 실행 모델 = **caucus 자체 멀티플렉서**. 장기 실행 풀스크린 TUI 프로세스. agent는 caucus가 관리하는 **패널별 PTY**에서 도는 `claude` / `codex` CLI 프로세스. tmux / zellij 의존 없음 — 둘은 분석 레퍼런스(kodex 적재)일 뿐. |
| 3 | VT 레이어 = **공개 크레이트** (경로 B-i): `vte`(escape-sequence 파서) + `portable-pty`(PTY 관리) + `ratatui`(패널 레이아웃·렌더). caucus가 직접 짜는 것은 grid `vte::Perform` 구현(~2-4k LOC) 하나뿐이며 zellij `zellij-server/src/panes/grid.rs`를 라인 단위 레퍼런스로 쓴다. zellij 크레이트 통째 vendor는 기각 — grid가 `output`/`tab`/`ui`/`route`/`screen`/`thread_bus` + `zellij-utils` ~140k LOC와 결합되어 깨끗한 추출 불가. |
| 4 | **main worker** = caucus 패널 중 하나에서 도는 주(主) 에이전트(Claude Code). 사용자가 직접 대화한다. main은 작업을 sub-task로 분해해 — 간단한 건 자기 패널에서 직접 처리하고, 병렬화 이득이 있는 건 **caucus MCP 서버**의 10개 툴(`send_keys` / `broadcast` / `ctrl_c` / `read_panel` / `spawn_role` / `kill_panel` / `list_panels` / `register_round` / `read_menu` / `select_option`)로 sub-agent 패널을 띄워 분배·관리·병합한다. 구 'CEO' 명칭은 폐기 — 수동적 보스가 아니라 직접 일하며 sub-agent를 지휘하는 메인 워커. |
| 5 | 턴 완료 신호 = Claude `Stop` hook이 **caucus 실행 프로세스의 소켓에 post**한다. 파일 sentinel(`*.sentinel.json`) + 폴링 watcher는 폐기. |
| 6 | "라운드" 개념은 유지하되 **라이브화** — 파일 안건 broadcast + `response-*.md` 수집 대신, CEO가 패널에 라이브로 키를 입력하고 §5의 턴 완료 신호로 라운드 진행을 판정한다. |
| 7 | Role 정의: `~/.caucus/roles.toml` (전역) + `<repo>/.caucus/roles.toml` (프로젝트 오버라이드, 우선). |
| 8 | 실행 단계 worktree per role 유지. Agent manifest 위치: `<repo>/.caucus/sessions/<session_id>/agents/<agent_id>.{md,json}`. `.gitignore`에 `.caucus/` 추가 권장. |
| 9 | agent 백엔드 CLI = **다중**: `claude` / `codex` / `gemini` 등. role의 `agent_cli` 필드로 선택. main은 `spawn_role` 시 모델과 백엔드 CLI를 **자체 판단으로** 지정한다 — caucus 코어는 메커니즘만 제공하고 어떤 모델/CLI를 쓸지는 main의 판단. |
| 10 | 패널은 **동적**. main이 `spawn_role` / `kill_panel` MCP 툴로 sub-agent 수를 늘이고 줄이며, caucus는 그때마다 레이아웃을 reflow한다. 고정 roster 가정 없음. |
| 11 | caucus 패널은 **완전한 양방향 인터랙티브 터미널**. 단순 관찰·제어가 아니라, 로그인 / OAuth 디바이스 코드 / 기타 대화형 프롬프트를 사용자가 패널에 직접 입력하거나 main이 `send_keys`로 처리할 수 있다. PTY 입력은 완전 양방향. |
| 12 | **토큰·효율 관리는 main의 자체 판단.** main은 패널별 토큰 사용량을 `read_panel`로 읽고, 필요 시 각 agent에 `/compact` · `/clear` 등 슬래시 커맨드를 `send_keys`로 보낸다. caucus 코어는 토큰 사용량을 노출만 하고 정책은 main이 결정한다. |
| 13 | **중첩 sub-agent 금지.** 어떤 agent도(main 포함) 자기 Claude Code / Codex 세션 안에서 `Task` 류 in-session sub-agent를 띄우지 않는다. 모든 sub-agent는 caucus가 관리하는 *관찰 가능한 패널*이어야 한다 — 보이지 않는 프로세스는 "모든 세션을 본다"는 caucus의 존재 이유를 깨뜨린다. 각 agent는 위임받은 작업을 자기 패널에서 직접 수행하고, main은 위임을 `Task`가 아니라 `spawn_role` + `send_keys`(패널 제어)로 한다. role의 `allowed_tools`에 `Task`를 포함하지 않는다. |
| 14 | **main은 화면을 실시간으로 경주하지 않는다.** 빠르게 스크롤되는 패널 출력은 *사람의 라이브 뷰*용이고, main은 caucus가 캡처한 영속 기록을 자기 페이스로 읽는다. caucus는 패널별 스크롤백 버퍼 + 턴 경계(`PromptDelivered`…`TurnCompleted`)로 구간된 append-only 출력 로그를 유지하며, `read_panel`은 `screen` / `scrollback` / `since_last_turn` / `last_message` 모드로 턴 출력 전체를 빠짐없이 돌려준다(§8.5). |
| 15 | sub-agent 모델 = **동적 병렬 워커**. main은 작업을 sub-task로 쪼개 동질적인 sub-agent를 병렬로 spawn하고 각자에 sub-task를 배분·관리·병합한다. 고정된 "전문가 팀"이 아니라 그때그때 필요한 만큼의 병렬 워커다. role(architect/backend/reviewer/…)은 *선택적 힌트*이며, 기본 sub-agent role은 범용 `worker`. 병렬 sub-agent는 worktree per agent(§5)로 격리하고 결과를 병합한다. |
| 16 | **sub-agent 컨텍스트 = lean brief + self-served depth.** main은 sub-agent에 *focused brief*(sub-task + `file:line` 포인터 + 제약 + 성공 기준)만 주고 대화 컨텍스트를 통째로 덤프하지 않는다. sub-agent는 깊은 코드베이스 지식을 **kodex 지식 그래프**에서 스스로 길어온다 — 내장 role의 `allowed_tools`에 `mcp__kodex` 포함(읽기 `recall`/`query` + 쓰기 `learn`, 사용자 결정). main은 *scope·intent*를, sub-agent는 *detail*을 담당해 모든 패널의 컨텍스트가 lean하게 유지된다. kodex는 사용자 글로벌 MCP 서버이며 `roles.toml`로 교체·제거 가능. |

본 결정은 v0에서는 변경 금지. v1 이후 재논의.

---

## 1. 시스템 다이어그램

```
        사용자
          │  키 입력 (caucus 풀스크린 TUI)
          ▼
┌──────────────────────────────────────────────────────────┐
│  caucus  (장기 실행 멀티플렉서 프로세스)                     │
│                                                            │
│  ┌──────────┬───────────┬──────────┬──────────┐            │
│  │ main     │ worker    │ worker   │ worker   │  ← 패널 =   │
│  │ (claude) │ (claude)  │ (codex)  │ (claude) │    agent,   │
│  └──────────┴───────────┴──────────┴──────────┘    PTY 1개씩 │
│       │           ▲           ▲          ▲                 │
│       │ MCP 툴 호출 │           │          │                 │
│       ▼           │ send_keys / ctrl_c / read_panel        │
│  ┌─────────────────────────────────────────┐               │
│  │ caucus 코어                               │               │
│  │  pty/    portable-pty 래퍼 (패널별 PTY)    │               │
│  │  term/   vte 기반 grid (Perform 구현)      │               │
│  │  render/ ratatui 패널 레이아웃·드로잉      │               │
│  │  input/  키 라우팅 (Ctrl-A 프리픽스 키맵)   │               │
│  │  mcp/    main worker용 MCP 서버            │               │
│  │  hook 소켓 ◄── 각 agent의 Claude Stop hook  │               │
│  └─────────────────────────────────────────┘               │
│                                                            │
│  agent/manifest · worktree/ · role/ · config/  (구 모델에서  │
│  유지되는 인프라)                                            │
└──────────────────────────────────────────────────────────┘

main worker는 (별도로) Notion MCP·kodex MCP를 자기 MCP 툴박스로 직접 호출 —
caucus 코어는 외부 API를 호출하지 않는다.
```

핵심 분리: **caucus 코어는 프레임**(PTY·grid·렌더·입력 라우팅·MCP 서버),
**main worker는 지능**(사용자 명령 해석·작업 분해·sub-agent 패널 제어·외부
sync). 사용자는 main worker 패널과 대화하고, main worker는 작업을 직접
처리하거나 MCP 툴로 sub-agent 패널을 띄워 분배·관리·병합한다.

---

## 2. 용어집

| 용어 | 의미 |
|---|---|
| **Session** | caucus 멀티플렉서 한 인스턴스. 한 주제를 두고 도는 패널들의 집합. |
| **Panel** | caucus 화면의 한 칸 = PTY 1개 + vte grid + 렌더 영역. tmux/zellij의 pane에 해당. |
| **Agent** | 한 Panel에서 도는 `claude` / `codex` / `gemini` CLI 프로세스. 한 Role의 한 인스턴스. |
| **Role** | main / worker / architect / backend / reviewer 등. system prompt + tool allowlist + 기본 `model`·`agent_cli` 정의. |
| **Main worker** | caucus 패널 중 하나에서 도는 주(主) Claude Code 에이전트. 사용자가 직접 대화한다. 간단한 일은 자기 패널에서 직접 처리하고, 병렬화 이득이 있는 일은 MCP 툴로 sub-agent 패널을 띄워 분배·관리·병합한다. 구 'CEO'를 대체. |
| **Sub-agent** | main worker가 `spawn_role`로 띄운 워커 패널. 기본 role은 범용 `worker`이고 specialist role은 선택적 힌트(§0 #15). |
| **Grid** | 한 패널의 vte-파싱된 화면 상태(셀 매트릭스 + 스크롤백). |
| **Turn signal** | agent가 한 턴을 마쳤다는 신호. Claude `Stop` hook이 caucus 소켓에 post. |
| **Round** | main worker가 여러 패널에 같은 안건을 라이브로 던지고 각 패널의 turn signal로 완료를 판정하는 한 묶음. |
| **Manifest** | 한 agent의 LaneEvent 타임라인 + derived_state + commit_provenance 영속화. |
| **Lane** | 한 agent의 작업 흐름. claw-code 용어 차용. |
| **MCP 서버** | caucus가 main worker에게 노출하는 제어 인터페이스 — 10개 툴(`send_keys` / `broadcast` / `ctrl_c` / `read_panel` / `spawn_role` / `kill_panel` / `list_panels` / `register_round` / `read_menu` / `select_option`). |
| **Session record** | 한 세션의 패널 roster 영속 스냅샷(`.caucus/sessions/<id>/session.json`). `caucus resume`가 이를 읽어 패널을 재생성. |

---

## 3. Session & Panel 상태머신

caucus는 장기 실행 프로세스다. Session = 그 프로세스가 들고 있는 패널 집합. 라이브
모델에서 "분해 / 병렬 실행 / 병합"은 CLI 전이가 아니라 main worker의 판단으로
흐르므로, 비동기 회의 모델의 무거운 상태머신은 사라진다.

**Session 상태** — 단순히 둘:

```
[active] ─────────────────────────────► [closed]
   패널 spawn/kill · 라운드 · 실행이          모든 패널 종료 또는
   전부 이 안에서 일어남                      사용자가 caucus 종료
```

진짜 lifecycle은 **Panel(=Agent) 단위**에 있다:

```
[spawning] ──► [live] ─────────► [exited]
                 │   ▲
          working │   │ idle      (turn signal로 working ⇄ idle 토글)
                 ▼   │
              [blocked]    (권한 프롬프트 / 머지 충돌 / 백그라운드 잡 —
                            grid 관찰로 감지, §8.3)
```

worktree 실행은 패널의 한 속성(`worktree_path`)일 뿐 별도 상태가 아니다.

**불변식**: Session 상태 전이는 `session::state::transition()`, Panel 상태 전이는
`panel::lifecycle::transition()` 단일 owner만 수행. 다른 모듈은 이벤트를 emit하고
소비자가 transition을 호출한다.

### 3.1 세션 영속화 & resume

caucus 세션은 본래 휘발성이다 — caucus가 종료되면 agent 프로세스도 죽는다.
agent별 manifest는 `agents/`에 남지만, *roster*(어떤 role이 어떤 순서로,
어떤 CLI/model/worktree/대화-id로 떠 있었나)를 기술하는 단일 파일은 없었다.
**Session record**가 그 파일이다 — `session::record::SessionRecord`.

[`crate::session::Multiplexer`]는 패널 roster가 바뀔 때마다
`.caucus/sessions/<session_id>/session.json`을 원자적으로 쓴다. 레코드에는
세션 id·topic·repo 경로·생성 시각·레이아웃 모드, 그리고 패널별로 role·`agent_cli`·
`model`·정렬 인덱스·worktree 브랜치·Claude 대화 id가 담긴다.

`caucus resume <session_id>`는 그 레코드를 읽어 패널을 재생성한다 — worktree
브랜치에 worktree를 다시 attach하고(`worktree::manager::attach`), Claude 대화는
`claude --resume <id>`로 이어붙인다. `caucus sessions`는 `.caucus/sessions/*/`를
스캔해 resume 가능한 레코드를 최신순으로 나열한다(§10).

---

## 4. Round 프로토콜 (라이브)

라운드 = main worker가 여러 sub-agent 패널에 sub-task를 라이브로 던지고, 각
패널의 turn signal로 완료를 감지한 caucus가 결과를 모아 main에 push하는 한
묶음. 파일 안건도, `response-*.md` 수집도 없다.

```
1. main worker가 MCP 툴 호출 — 작업을 sub-task로 분해해 병렬 분배한 뒤
   라운드를 등록하고 자기 턴을 닫는다:
     broadcast(panels=[<worker-1>,<worker-2>,<worker-3>], text="<공통 안건>", enter=true)
     (패널별로 다른 sub-task면 send_keys를 패널마다)
     register_round(panels=[<worker-1>,<worker-2>,<worker-3>])
   register_round는 즉시 반환한다 — 블로킹도 timeout 대기도 없다. main worker는
   턴을 끝내고 자유로워진다.

2. 각 패널의 sub-agent가 작업 → 턴 종료 시 Claude Stop hook이 caucus 소켓에
   turn signal을 post (panel_id, last_message 포함, §7).

3. caucus가 turn signal 수신 → 해당 패널 manifest에 TurnCompleted LaneEvent
   append → derived_state를 idle로. caucus는 매 tick `poll_pending_rounds`로
   등록된 라운드의 패널이 모두 settle했는지 확인한다.

4. 라운드의 패널이 모두 idle이 되면 caucus가 각 패널 결과(read_mode 기본
   last_message, since_last_turn도 가능)를 모아 **main worker 패널에 새 turn으로
   주입**한다 — pull-only MCP가 못 하는 caucus→main push다(§8.5). main worker가
   idle이고 사용자가 입력 중이 아닐 때 전달된다. 안전망: fallback_secs가 지나면
   미완 패널을 "still working"으로 표시해 부분 결과를 전달한다.

5. main worker가 주입된 라운드 결과를 받고 판단:
     - 라운드 더  → 새 안건으로 broadcast + register_round 반복
     - 병합 완료  → 결과를 종합해 사용자에게 보고
     - 막힘      → 사용자에게 보고
```

**선택 프롬프트 엣지**: 라운드 패널의 sub-agent가 턴을 끝내지 않고 대화형
선택 메뉴(AskUserQuestion 류)에서 멈추면 turn signal이 안 와 라운드가 settle하지
못한다 — 그대로 두면 fallback_secs까지 멈춰 있다. caucus는 매 tick
`poll_round_selection_prompts`로 라운드 패널의 메뉴를 감지해 main worker에 push
알림을 보낸다(round 완료 push와 같은 idle·quiet 게이트, tick당 1건). main은
`read_menu`/`select_option`으로 답해 패널을 풀어 준다 — §8.3.

라이브 모델에는 `max_rounds` 같은 강제 캡이 없다 — main worker가 자기 토큰
예산(§0 #12)에 맞춰 라운드를 몇 번 돌지 스스로 판단한다.

**순차 의존**이 필요하면 main worker가 선행 패널에 `send_keys` → 그 패널만으로
`register_round` → 완료 push를 받은 뒤 후행 패널에 `send_keys` 하면 된다. caucus
코어에 별도 `--lead` 메커니즘은 필요 없다.

---

## 5. 실행 단계

코드를 실제로 쓰는 단계. 병렬 sub-agent는 worktree per agent로 격리한다(§0 #15).

```
1. main worker가 MCP 툴 호출 — 병렬 sub-agent를 띄움:
     spawn_role(role="worker", worktree=true, model="sonnet")
   → caucus가:
     - <repo>/.caucus/worktrees/<session>-worker-NN/ worktree 생성
     - 그 worktree를 cwd로 새 패널 spawn (agent_cli = claude/codex/gemini)
     - 레이아웃 reflow
     → 응답: {panel_id, worktree_path}

2. main worker가 send_keys로 sub-task를 지시. sub-agent가 코드 작성 + 커밋.
   턴 종료 시 turn signal (last_message에 git log 포함 가능).

3. caucus가 turn signal의 last_message에서 commit SHA를 추출
   (`provenance::extract_commit_sha`) → manifest의 commit_provenance에 기록.

4. main worker 판단:
     - 통과 → worktree 결과를 병합하고 사용자에게 브랜치명 보고
              (caucus는 자동 머지 안 함)
     - 폐기 → kill_panel(panel_id) → caucus가 worktree를 cleanup 큐에 enqueue
```

**worktree 정리**: `kill_panel`이 worktree 패널을 죽이면 `worktree::cleanup::enqueue()`로
직렬 큐에 들어간다. UI를 막지 않는다.

caucus는 머지를 자동 수행하지 않는다 — 사람의 결정이다. main worker는 worktree
브랜치명을 사용자에게 알리고, 사용자가 준비되면 머지한다.

---

## 6. Role 정의

`~/.caucus/roles.toml` (전역):

```toml
[roles.main]
description = "Main worker. Talks to the user, does small work itself, spawns sub-agents for parallel work"
allowed_tools = ["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite", "WebFetch", "WebSearch"]
permission_mode = "default"
system_prompt_template = "roles/main.md"
agent_cli = "claude"
model = "opus"

[roles.worker]
description = "Generic sub-agent. The default parallel worker spawned by the main worker"
allowed_tools = ["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite"]
permission_mode = "acceptEdits"
system_prompt_template = "roles/worker.md"
agent_cli = "claude"
model = "sonnet"

[roles.architect]
description = "Designs the approach, decomposes tasks, no code edits"
allowed_tools = ["Read", "Glob", "Grep", "WebFetch", "WebSearch", "TodoWrite"]
permission_mode = "plan"
system_prompt_template = "roles/architect.md"
agent_cli = "claude"             # claude | codex | gemini  (생략 시 claude)
model = "opus"                   # CLI tier alias 또는 pinned 버전 (생략 시 CLI 기본)

[roles.backend]
description = "Implements changes. Has full file edit + bash."
allowed_tools = ["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite"]
permission_mode = "acceptEdits"
system_prompt_template = "roles/backend.md"
agent_cli = "claude"
model = "sonnet"

[roles.reviewer]
description = "Reads only. Critiques approach + code."
allowed_tools = ["Read", "Glob", "Grep", "Bash"]   # Bash는 cargo check 등을 위해
permission_mode = "default"
system_prompt_template = "roles/reviewer.md"
agent_cli = "claude"
model = "opus"

[roles.serious-reviewer]
description = "Adversarial second-opinion reviewer on a different model."
allowed_tools = ["Read", "Glob", "Grep", "Bash"]
permission_mode = "default"
system_prompt_template = "roles/serious-reviewer.md"
agent_cli = "codex"              # claude가 막히거나 rubber-stamp할 때 다른 모델로 반론
```

`main`은 사용자가 직접 대화하는 오케스트레이터 패널이고, `worker`는 main worker가
병렬 작업을 위해 띄우는 **기본 sub-agent**다(§0 #15). 나머지 role(architect /
backend / reviewer / qa / scribe / serious-reviewer)은 *선택적 specialist 힌트*로
남는다 — sub-task가 명확히 그 전문성을 요구할 때만 쓴다.

`agent_cli` 미지정 시 `claude`. `model` 미지정 시 해당 CLI 기본. main worker는
`spawn_role` 호출 시 role의 기본 `model`·`agent_cli`를 무시하고 자체 판단으로
덮어쓸 수 있다(§0 #9) — 예: 같은 `worker` role을 토큰 절약을 위해 `gemini`로,
또는 어려운 라운드는 `opus`로.

`<repo>/.caucus/roles.toml`로 프로젝트별 오버라이드 가능. 같은 role 이름이면 프로젝트가 우선.

**`allowed_tools`에 `Task`를 넣지 않는다(§0 #13).** agent가 자기 세션 안에서
보이지 않는 sub-agent를 띄우면 caucus가 관찰할 수 없다. 모든 sub-agent는
패널이어야 하므로, 위임은 main worker가 `spawn_role`로 새 패널을 만들어 수행한다.
`caucus doctor`는 role 정의에 `Task`가 있으면 경고한다.

### 6.1 System prompt template (claw-code 4-제약 + role-specific)

`roles/reviewer.md` 예:

```markdown
You are a `reviewer` sub-agent in a caucus session.

# Universal constraints (claw-code subagent scaffolding)
- Work only on the delegated task.
- Use only the tools available to you.
- Do not ask the user questions; if blocked, state your block reason and end your turn.
- Finish with a concise result.

# Role-specific
- You may NOT write or edit code. You may read, search, and run `cargo check` /
  `cargo test --no-run` to validate compilability.
- Reply in your own terminal — the main worker reads your panel directly (no response file).
  Structure your review as:
  - Findings (numbered list, file:line citations)
  - Risks (each with severity: blocker / high / medium / low)
  - Recommendation: (approve | request_changes | block)
  - Open questions for the main worker
- If you find an issue, cite the anchor pattern (rg regex) so others can audit the
  rest of the codebase per the CLAUDE.md "Fixes from reported defects" rule.
```

`roles/main.md`, `roles/worker.md`, `roles/architect.md` 등도 같은 패턴.
role-specific 가이드만 다름. 응답이 *자기 터미널*로 간다는 점이 구 모델(response
파일)과의 핵심 차이다 — main worker가 `read_panel`로 직접 읽는다.

---

## 7. Turn-completion 신호 (Stop hook → 소켓)

agent가 한 턴을 마쳤음을 caucus가 *라이브로* 아는 메커니즘. 파일 sentinel +
폴링 watcher는 폐기됐다(§0 #5).

### 7.1 채널
caucus는 기동 시 unix domain socket을 연다:
`<repo>/.caucus/sessions/<session_id>/caucus.sock`. 패널을 spawn할 때 caucus는
그 agent 프로세스에 env를 주입한다 — `CAUCUS_SESSION_ID`, `CAUCUS_PANEL_ID`,
`CAUCUS_SOCK`.

### 7.2 작성 주체
**Claude `Stop` hook**. caucus는 이 hook을 `caucus init --install-hook`으로
설치한다 — Claude `~/.claude/settings.json`에 merge하고 `.bak` 백업을 남긴다.

### 7.3 hook 스크립트
`caucus init`이 만드는 `.caucus/bin/turn-signal`:

```sh
#!/bin/sh
# CAUCUS_* env는 caucus가 패널 spawn 시 주입. Claude hook payload는 stdin JSON.
exec caucus signal post \
  --sock    "$CAUCUS_SOCK" \
  --session "$CAUCUS_SESSION_ID" \
  --panel   "$CAUCUS_PANEL_ID" \
  --kind    stop
```

`caucus signal post`는 stdin JSON에서 `last_message`를 추출해 소켓에 한 줄 JSON으로
쓴다. 파일도, 폴링도 없다 — caucus 실행 프로세스가 소켓 read로 즉시 수신한다.

### 7.4 turn signal 스키마

```json
{
  "session_id": "01HXX...",
  "panel_id": "01HXY...",
  "ts": "2026-05-16T14:23:01Z",
  "kind": "stop",
  "last_message": "Completed reviewer pass. 3 findings.",
  "raw_hook_payload": { "...": "..." }
}
```

`kind`: `stop | tool_blocked | error`.

**Codex / Gemini 백엔드**: 동등한 turn-completion hook이 있으면 같은 스크립트를
재사용한다. hook을 노출하지 않는 백엔드는 caucus가 grid 관찰(agent 프롬프트 복귀
패턴 매치)로 fallback한다 — §8.3. 휴리스틱이라 hook 경로보다 신뢰도가 낮다.

---

## 8. Manifest & LaneEvent

### 8.1 AgentManifest 스키마

```json
{
  "agent_id": "01HXY...",
  "session_id": "01HXX...",
  "role": "reviewer",
  "agent_name": "reviewer-r1",
  "panel_id": "01HXY...",
  "agent_cli": "claude",
  "worktree_path": null,
  "model": "opus",
  "status": "live",
  "created_at": "2026-05-16T14:20:00Z",
  "started_at": "2026-05-16T14:20:02Z",
  "exited_at": null,
  "lane_events": [
    { "kind": "started", "ts": "2026-05-16T14:20:02Z" }
  ],
  "current_blocker": null,
  "derived_state": "working",
  "error": null
}
```

`tmux_pane_id` → `panel_id`로 교체, `agent_cli` 필드 추가(claude/codex/gemini).

### 8.2 LaneEvent 종류 (claw-code 차용 + caucus 확장)

```rust
enum LaneEventKind {
    Started,
    PromptDelivered,   // main worker가 send_keys로 안건 전달
    TurnCompleted,     // Stop hook turn signal 수신
    Blocked { blocker: LaneEventBlocker },
    Failed  { blocker: LaneEventBlocker },
    Finished { detail: String },
    CommitCreated { provenance: LaneCommitProvenance },
    WorktreeCreated { path: PathBuf },
    WorktreeRemoved { path: PathBuf },
}
```

`SentinelReceived` → `TurnCompleted`로 교체, `ResponseFileWritten`은 제거(라이브엔
응답 파일이 없다).

### 8.3 derived_state

```
working                     (PromptDelivered 후, 다음 turn signal 전)
idle                        (turn signal 수신 — 다음 지시 대기)
awaiting_selection          (grid에 화살표-탐색 선택 메뉴 — turn signal 없이 중단, §8.3)
blocked_permission_prompt   (grid에 권한 프롬프트 정규식 매치)
blocked_merge_conflict
blocked_background_job
degraded_mcp
interrupted_transport
exited
```

파일 기반 `finished_cleanable` / `finished_pending_report`는 제거 — 라이브엔 응답
파일이 없다. turn signal 수신 = `idle`, 다음 `PromptDelivered`부터 다시 `working`.

`blocked_permission_prompt`: turn signal 없음 AND 패널 grid에
`Allow this tool? [y/n]` 류 정규식 매치. main worker에 알림 — 자동 yes는 안 한다(위험).

`awaiting_selection`: 패널 grid에 AskUserQuestion 류 선택 메뉴(`❯` 커서 + footer
`Enter to select · ↑/↓ to navigate`)가 보일 때 — agent가 turn을 끝내지 않고 선택을
기다리며 멈춘 상태라 turn signal이 안 온다. `term::prompt_scan`이 메뉴를 파싱하고,
`list_panels` 읽는 시점에 `working`/`idle` 위로 덮어씌운다(더 강한 상태는 안 가린다).
이 패널이 라운드 소속이면 caucus가 매 tick `poll_round_selection_prompts`로 감지해
main worker에 push 알림을 보낸다(라운드는 settle 못 하므로 — §4). main은 `read_menu`로
선택지를 읽고 `select_option(panel, n)`으로 답한다(caucus가 화살표 n칸 + Enter 주입);
자유 입력은 메뉴의 'type something' 옵션을 고른 뒤 `send_keys`. `list_panels` 표면 문자열은
`awaitingselection`(Debug-lowercase).

turn-completion hook이 없는 백엔드(codex/gemini가 hook 미지원 시): caucus가 grid
관찰로 `idle`을 판정한다 — agent 프롬프트 복귀 패턴 매치. 휴리스틱이므로 hook
경로보다 신뢰도가 낮다.

### 8.4 derive 함수

```rust
fn derive_agent_state(
    status: &str,
    last_turn_signal: Option<&TurnSignal>,
    error: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
    grid_hint: Option<&GridHint>,
) -> DerivedState
```

turn signal 수신 또는 grid 변화 시 재계산.

### 8.5 패널 출력 캡처 — main worker가 화면을 경주하지 않게

**문제**: agent 출력은 빠르게 스크롤된다(툴 출력·diff·추론). main worker는
라이브로 화면을 보는 게 아니라 이산적인 MCP 호출로 동작하므로, `read_panel`이
보이는 grid(뷰포트 수십 줄)만 돌려주면 스크롤로 지나간 내용을 전부 놓친다.

**해결** — caucus는 main worker가 화면을 경주하게 두지 않는다:

1. **스크롤백 버퍼.** `term/`은 패널별로 뷰포트가 아닌 bounded 스크롤백 링을
   유지한다(zellij/tmux와 동일).
2. **턴 단위 출력 로그.** caucus는 패널 PTY 출력을 턴 경계로 구간해 append-only로
   캡처한다 — `PromptDelivered`부터 `TurnCompleted`까지가 한 턴.
   `.caucus/sessions/<id>/panels/<panel_id>.log`(메모리 링 + 디스크 spill). 라운드가
   settle하면 caucus가 이 캡처에서 각 패널 결과를 모아 main 패널에 push하고(§4),
   main worker는 더 필요한 디테일을 `read_panel`로 자기 페이스로 읽는다 — 경주 없음.
3. **`read_panel` 모드.** MCP `read_panel` 툴은 `mode`를 받는다:
   - `screen` — 현재 보이는 grid
   - `scrollback` — 스크롤백 버퍼 전체
   - `since_last_turn` — 마지막 `PromptDelivered` 이후 출력 전체 ("이 agent가 방금
     한 일"의 자연스러운 단위)
   - `last_message` — turn signal이 실어온 agent 최종 메시지만(§7.4)
4. **turn signal이 이미 결론을 실어온다.** Claude Stop hook payload는 최종
   assistant 메시지를 포함하므로(`last_message`), 대부분의 main worker 판단은
   터미널 스크래핑 없이 끝난다. `since_last_turn`은 중간 디테일(어떤 파일이
   바뀌었나, 툴이 무엇을 출력했나)이 필요할 때만 쓴다.

빠르게 지나가는 화면은 사람이 곁눈질로 보는 용도다. main worker의 진실 공급원은
caucus의 영속 캡처(스크롤백 + 턴 로그)다.

---

## 9. 모듈 구조

```
caucus/
├── Cargo.toml
├── README.md
├── docs/
│   ├── design.md            (본 문서)
│   ├── dmux-analysis.md
│   └── claw-code-analysis.md
├── roles/                   (system prompt templates)
└── src/
    ├── main.rs              (진입점: `caucus` = TUI 기동 / 비-TUI 서브커맨드 분기)
    ├── lib.rs               (라이브러리 노출, 테스트용)
    ├── cli.rs               (init/doctor/signal/role/sessions/resume/mcp-serve 비-TUI 서브커맨드 dispatch)
    ├── config/              (글로벌+프로젝트 config 병합, roles.toml 파싱)
    ├── session/
    │   ├── state.rs         (Session 상태머신, transition() 단일 owner)
    │   ├── id.rs            (ULID 발급)
    │   └── record.rs        (SessionRecord 영속화 — session.json, `caucus resume` 데이터 소스)
    ├── role/
    │   ├── registry.rs      (이름 → RoleSpec 조회)
    │   └── spec.rs          (RoleSpec: allowlist, prompt_template, permission_mode, agent_cli, model)
    ├── pty/                 (portable-pty 래퍼: 패널별 PTY spawn/read/write/resize/kill)
    ├── term/                (vte 기반 grid: `Perform` 구현, 셀 매트릭스, 스크롤백)
    ├── render/              (ratatui: 패널 레이아웃, reflow, 드로잉, focus 표시)
    ├── input/               (키 라우팅: `Ctrl-A` 프리픽스 키맵, focus·임의 키 → PTY, §9.2)
    ├── panel/
    │   └── lifecycle.rs     (Panel 구조체 + spawn/kill + 레이아웃 reflow, transition 단일 owner)
    ├── mcp/                 (main worker용 MCP 서버: 10개 툴 — send_keys/broadcast/ctrl_c/read_panel/spawn_role/kill_panel/list_panels/register_round/read_menu/select_option)
    ├── signal/              (turn-signal 소켓 서버 + `caucus signal post` 클라이언트)
    ├── agent/
    │   ├── spawn.rs         (RoleSpec → 새 패널 + 새 AgentManifest)
    │   ├── manifest.rs      (AgentManifest 영속화: .json + .md 페어)
    │   ├── lane_event.rs    (LaneEvent enum + append)
    │   ├── derive_state.rs  (turn signal + grid_hint → DerivedState)
    │   └── provenance.rs    (extract_commit_sha + git rev-parse → LaneCommitProvenance)
    ├── worktree/
    │   ├── manager.rs       (생성)
    │   └── cleanup.rs       (직렬 큐 + depth-desc 정렬, tokio::mpsc consumer)
    └── doctor.rs            (git/claude/codex/gemini/hook + role allowlist `Task` 점검)
```

**폐기된 모듈**: `tmux/`(자체 멀티플렉서로 대체), `sentinel/`(turn-signal 소켓으로
대체), `status/poller`(라이브 turn signal로 불필요), `notify/`(소켓이 곧 알림),
`round/`·`consensus/`·`execute/lifecycle`(라운드·병합·실행은 이제 caucus 모듈
lifecycle이 아니라 main worker가 MCP 툴로 라이브 수행). worktree 생성/정리만 모듈로 남는다.

### 9.1 모듈 owner 매트릭스 (불변식 enforcement)

| 자원 | 단일 owner | 규칙 |
|---|---|---|
| Session state 전이 | `session::state::transition()` | 다른 모듈은 event만 emit |
| Panel lifecycle 전이 | `panel::lifecycle::transition()` | 직접 status 변경 금지 |
| AgentManifest 작성 | `agent::manifest::write()` | 외부 직접 write 금지 |
| PTY 생성 / 종료 | `pty::Pty::spawn()` / `kill()` | 직접 `openpty`/`fork` 금지 |
| Panel 생성 / 종료 | `panel::lifecycle::spawn()` / `kill()` | 직접 PTY·패널 벡터 조작 금지 |
| Grid 변경 | `term::Grid` (`vte::Perform`) | PTY 바이트만 입력, 외부 직접 셀 변경 금지 |
| Turn signal 수신 | `signal::server::ingest()` | 직접 소켓 read 금지 |
| worktree 생성 | `worktree::manager::create()` | 직접 `git worktree add` 금지 |
| worktree 삭제 | `worktree::cleanup::enqueue()` | 직접 삭제 금지. 직렬 큐를 통해서만 |
| Notion / kodex 호출 | **caucus 안 함** | main worker만 자기 MCP로 호출 |

각 모듈은 외부에 노출하는 함수 외엔 `pub(crate)` 미만으로 잠금. Rust visibility로 강제.

### 9.2 키맵 · 레이아웃 제어 · transcript 오버레이 · 스크롤백 페이저

caucus는 단 하나의 **프리픽스 키** `Ctrl-A`를 자기 명령용으로 예약한다. 그 외
모든 키 입력은 — `Ctrl-C` 포함 — 터미널 바이트로 인코딩되어 focus 패널의 PTY로
그대로 forward된다(§0 #11). 프리픽스 키맵은 `input::FocusRouter`가 소유한다:

| 키 | 동작 |
|---|---|
| `Ctrl-A` 다음 `n` / `→` | 다음 패널로 focus |
| `Ctrl-A` 다음 `p` / `←` | 이전 패널로 focus |
| `Ctrl-A` 다음 `q` | caucus 종료 |
| `Ctrl-A` 다음 `z` | focus 패널 zoom 토글 |
| `Ctrl-A` 다음 `<` | focus 패널을 순서상 한 칸 앞으로 |
| `Ctrl-A` 다음 `>` | focus 패널을 순서상 한 칸 뒤로 |
| `Ctrl-A` 다음 `Space` | 레이아웃 배치 모드 순환 |
| `Ctrl-A` 다음 `t` | transcript 오버레이 토글 |
| `Esc` (오버레이 열림) | transcript 오버레이 닫기 |
| `Ctrl-A` 다음 `[` | focus 패널 스크롤백 페이저 열기 |
| (페이저 열림) `↑↓ k j` / `PgUp PgDn` | 한 줄 / 한 페이지 스크롤 |
| (페이저 열림) `g G` / `Home End` | 가장 오래된 / 최신 줄로 점프 |
| (페이저 열림) `Esc` / `q` | 페이저 닫기 |
| `Ctrl-A` 다음 `Ctrl-A` | 패널에 리터럴 `Ctrl-A` 전송 |

프리픽스는 소비된다 — `Ctrl-A` 다음 키는 명령을 선택하고 forward되지 않으며,
예외인 `Ctrl-A Ctrl-A`만 리터럴 `Ctrl-A`(0x01) 한 바이트를 패널로 보낸다.

**레이아웃 모드.** `render::LayoutMode`는 4종 — `Tiled` → `EvenHorizontal` →
`EvenVertical` → `MainVertical` → (순환). `Ctrl-A Space`가 다음 모드로 cycle하고
caucus는 패널을 해당 배치로 reflow한다. 패널 동적 spawn/kill 시에도 현재 모드로
reflow된다(§0 #10).

**transcript 오버레이.** `Ctrl-A t`는 읽기 전용 팀 관찰 뷰를 토글한다 —
패널 위에 그려지는 bordered 박스로, 패널당 한 행(role · derived_state · 완료
턴 수 · worktree 브랜치 · agent 최종 메시지 첫 줄)을 보여준다(`render::TranscriptRow`).
오버레이는 입력을 가로채지 않는다 — 열려 있어도 `Esc` 외의 모든 키는 focus
패널로 그대로 통과하고, `Esc`만 오버레이를 닫는다.

**스크롤백 페이저.** `Ctrl-A [`는 focus 패널의 터미널 스크롤백(grid 링, 기본
10,000행, §8.5)을 tmux copy-mode 식으로 보여주는 읽기 전용 전체 화면 뷰를
연다(`render::draw_scroll_pager`, 스냅샷·윈도잉은 `runtime::ScrollState` +
순수 헬퍼 `render::scroll_window`). 진입 시 최신 줄에서 열리고 `↑↓ k j`(줄)·
`PgUp/PgDn`(페이지)·`g/Home`(최古)·`G/End`(최新)으로 이동, `Esc/q`로 닫는다.
transcript 오버레이와 달리 페이저는 입력을 **가로챈다** — 키는 스크롤만 구동하고
PTY로는 한 바이트도 가지 않는다(`FocusRouter::scroll_open` 게이트). 보여주는 것은
진입 시점의 *고정 스냅샷*이다 — 패널은 밑에서 계속 돌고, 새 출력은 페이저를 닫은
뒤에 보인다. 소스는 bounded grid 스크롤백이며, unbounded 턴 로그/디스크 spill
페이징(`Turn N` 구분자 포함)은 향후 확장이다.

---

## 10. CLI surface

caucus는 이제 장기 실행 TUI다. 라이브 제어(`send_keys` / `spawn_role` …)는 CLI가
아니라 **MCP 서버**로 main worker에 노출된다(§0 #4). CLI는 기동·부트스트랩·hook·
세션 inspect/resume용으로 축소됐다 — 옛 `session` / `round` / `execute` / `agent` /
`watch` 서브커맨드군은 전부 폐기.

```
caucus                              # 풀스크린 멀티플렉서 TUI 기동 (현 git repo 기준).
                                    # main worker 패널 하나로 시작.
caucus --roles architect,backend,reviewer
                                    # 기동 시 초기 패널 구성까지 (생략 시 main worker 패널만)

caucus init [--install-hook]        # .caucus/ + bin/turn-signal 생성,
                                    # --install-hook 시 Claude Stop hook을
                                    # ~/.claude/settings.json에 merge
caucus doctor                       # git/claude/codex/gemini/hook + role allowlist `Task` 점검
caucus role list                    # 알려진 role 나열
caucus role show <name>             # 한 role의 전체 spec 출력
caucus sessions [--format json]     # resume 가능한 세션 나열 (최신순; §3.1)
caucus resume <session_id>          # 영속된 세션을 복원해 TUI 재기동 (§3.1)

caucus signal post --sock <s> --session <id> --panel <id> --kind stop
                                    # turn-signal hook이 호출 (사람은 안 침, 내부용)
caucus mcp-serve --control-sock <p> # main worker용 stdio MCP 서버
                                    # (main worker의 Claude Code가 spawn, 내부용)
```

TUI 안에서 사용자는 main worker 패널과 대화하고, main worker가 MCP 툴로 sub-agent
패널을 조종한다. 사용자가 직접 패널 focus를 옮기고 키를 입력할 수도 있다(§0 #11).

### 10.1 exit code 규약

비-TUI 서브커맨드(`init` / `doctor` / `signal` / `role` / `sessions`)에 적용:

- `0` — 성공
- `2` — 사용자 오류 (잘못된 인자 등)
- `3` — 환경 오류 (git 없음, claude CLI 없음 등)
- `4` — caucus 상태 비정상 (manifest 손상 등) — `caucus doctor` 권유
- `1` — 예상 못한 실패 (panic 등). bug로 간주.

### 10.2 stdout / stderr

비-TUI 서브커맨드는 정형 데이터를 stdout(JSON, `--format json` 시), 사람 읽기
메시지를 stderr(텍스트)로 분리한다. TUI 모드에는 해당 없음 — 화면이 곧 출력이다.

---

## 11. main worker 워크플로 (실제 사용 시나리오)

사용자는 caucus TUI를 띄우고 **main worker 패널과 자연어로 대화**한다. main
worker는 작업을 sub-task로 분해해 — 간단한 건 자기 패널에서 직접 처리하고,
병렬화 이득이 있는 건 MCP 툴로 sub-agent 패널을 띄워 분배·관리·병합한다. main
worker는 `Task` 류 in-session sub-agent를 절대 쓰지 않는다 — 모든 sub-agent는
`spawn_role`로 만든 관찰 가능한 패널이다(§0 #13).

### 시나리오: "epics-archiver의 write_loop를 리팩토링하자"

```text
$ cd ~/codes/archiver-rs && caucus
  → caucus TUI 기동. main worker 패널 하나.

사용자 → main worker 패널: "write_loop를 모듈 3개로 쪼개서 다시 짜자."

main worker: 작업을 sub-task로 분해 —
  - 간단·순차 부분(설계 스케치, 인터페이스 정의)은 자기 패널에서 직접 처리.
  - 모듈 3개 구현은 서로 독립적 → 병렬화 이득. sub-agent로 fan-out.

main worker → MCP (병렬 sub-agent 3개 spawn — 각자 worktree 격리):
  spawn_role(role="worker", worktree=true, model="sonnet")  → {panel=W1, worktree=...}
  spawn_role(role="worker", worktree=true, model="sonnet")  → {panel=W2, worktree=...}
  spawn_role(role="worker", worktree=true, model="sonnet")  → {panel=W3, worktree=...}
  → caucus가 패널 3개 추가, 레이아웃 reflow.

main worker → MCP (sub-task 배분 — 각 텍스트는 main worker가 직접 작성):
  send_keys(panel=W1, text="<모듈 A 구현 지시 + 인터페이스>", enter=true)
  send_keys(panel=W2, text="<모듈 B 구현 지시 + 인터페이스>", enter=true)
  send_keys(panel=W3, text="<모듈 C 구현 지시 + 인터페이스>", enter=true)

  (각 패널 sub-agent가 코드 작성 + 커밋 → Stop hook turn signal → caucus idle 표시)

main worker → MCP (모니터링):
  list_panels()
  # → {W1:"idle", W2:"idle", W3:"working"}
  ...W3까지 idle 되면:
  read_panel(W1, mode="since_last_turn"); read_panel(W2, ...); read_panel(W3, ...)
  → main worker가 각 sub-agent 결과(commit 포함)를 읽고 병합 판단.

main worker: 어려운 sub-task 하나가 막혔으면 specialist 힌트로 재시도 —
  spawn_role(role="serious-reviewer", model="opus")  # 까다로운 검토는 specialist로

  # 토큰이 커진 패널은 main worker 판단으로 정리:
  send_keys(panel=W1, text="/compact", enter=true)
  # 더 이상 필요 없는 패널:
  kill_panel(W1)

main worker → 사용자: "모듈 3개 구현 완료, worktree 브랜치 <a>/<b>/<c>.
                       병합할까요?"

main worker → (자기 MCP 툴박스로) Notion / kodex 동기화 — caucus 코어는 모름.
```

**caucus 코어는 Notion/kodex를 모른다.** main worker Claude가 자기 MCP 툴박스로
처리한다. 모든 sub-agent가 화면에 보이는 패널이고, main worker의 모든 제어가 MCP
툴 한 줄씩으로 일어나 사용자가 흐름 전체를 관찰할 수 있다는 점이 이 워크플로의
핵심이다.

---

## 12. 불변식 (CLAUDE.md 스타일)

각 불변식은 owner + 강제 메커니즘 명시.

### Invariant I-1: Session 상태 전이는 단일 owner를 통해서만
- **Owner**: `session::state::transition()`
- **MUST**: 모든 상태 전이는 이 함수를 거침.
- **MUST NOT**: 다른 모듈이 `session.state =` 직접 변경.
- **Enforcement**: `Session.state` 필드는 `pub(crate)`, mutate 함수 `pub(crate) fn`. 외부 crate는 못 건드림.
- **Tests**: 전이 함수에 모든 합법 전이 + 모든 불법 전이(reject) 테스트.

### Invariant I-2: Manifest write는 단일 owner를 통해서만
- **Owner**: `agent::manifest::write()` (and `dedupe_superseded_commit_events` 내부에서만 호출됨)
- **MUST**: LaneEvent append든 status 변경이든 manifest 변경은 이 함수만.
- **MUST NOT**: 다른 모듈이 manifest JSON 직접 write.
- **Enforcement**: `AgentManifest` 필드 `pub(crate)`, `to_disk()` 메서드만 `pub(crate)`. 다른 코드는 mutable borrow 못 가짐.
- **Tests**: concurrent append (두 LaneEvent가 동시에) 시 마지막 write가 모두 반영되는지 (read-modify-write 순서 보장).

### Invariant I-3: Worktree 삭제는 cleanup queue를 통해서만
- **Owner**: `worktree::cleanup::enqueue()` → `worktree::cleanup::run_one()`
- **MUST**: `git worktree remove`는 cleanup task 안에서만.
- **MUST NOT**: ad-hoc `git worktree remove` 호출 금지.
- **Enforcement**: `worktree::cleanup::run_one()`은 module-private. 외부는 `enqueue()`만 호출 가능.
- **Tests**: nested worktree 시나리오에서 depth-desc 순서로 삭제되는지.

### Invariant I-4: caucus 코어는 Notion / kodex 호출 금지
- **Owner**: (없음 — 부재가 invariant)
- **MUST**: caucus crate의 `Cargo.toml`에 `reqwest`, `tonic`, `kodex` 등 외부 sync용 dep 없음.
- **MUST NOT**: 어느 코드 경로도 외부 sync API 호출.
- **Enforcement**: `Cargo.toml` deny 리스트 + CI에서 `cargo tree | grep -E "(reqwest|tonic)"` 가 비어있는지 검사.

### Invariant I-5: Panel·PTY lifecycle은 단일 owner를 통해서만
- **Owner**: `panel::lifecycle` (spawn/kill), `pty::Pty` (PTY 생성/종료)
- **MUST**: PTY spawn/kill, 패널 추가/제거는 이 모듈만 수행.
- **MUST NOT**: 다른 모듈이 직접 `openpty`/`fork`를 호출하거나 패널 벡터를 mutate.
- **Enforcement**: `pty` / `panel` 내부 타입은 `pub(crate)`, 생성자 비공개.
- **Tests**: 동적 spawn/kill 후 레이아웃 reflow 일관성, PTY fd 누수 없음.

### Invariant I-6: Turn signal은 signal::server만 ingest
- **Owner**: `signal::server::ingest()`
- **MUST**: 소켓에서 온 turn signal은 이 함수만 파싱하고 manifest에 반영.
- **MUST NOT**: 다른 모듈이 turn-signal 소켓을 직접 read.
- **Enforcement**: 소켓 listener는 `signal::server` 내부에만 존재.
- **Tests**: 동시 turn signal 수신 시 순서·manifest 반영 보장.

### Invariant I-7: 중첩 sub-agent 금지 — 모든 팀원은 패널
- **Owner**: (없음 — 부재가 invariant, §0 #13)
- **MUST**: 모든 agent는 caucus가 spawn한 패널에서만 존재. 위임은 `spawn_role`로.
- **MUST NOT**: 어떤 role의 `allowed_tools`에도 `Task`를 포함. agent가 in-session sub-agent를 띄움.
- **Enforcement**: role 로더가 `Task`를 거부, `caucus doctor`가 role 정의의 `Task`를 경고.
- **Tests**: `Task` 포함 `roles.toml` 로드 시 거부/경고되는지.

---

## 13. 스코프와 non-goals

### 스코프 안에 들어 있는 것 (v0 + v1)
- caucus 멀티플렉서 TUI — 풀스크린, 패널별 PTY(`portable-pty`), `vte` 기반 grid, `ratatui` 렌더
- 패널 동적 spawn / kill + 레이아웃 reflow
- 입력 라우팅 — focus 패널 전환, `Enter` / `Ctrl-C` / 임의 키, 완전 양방향 인터랙티브 입력(로그인·OAuth 흐름 포함)
- main worker용 MCP 서버 — 10개 툴: `send_keys` / `broadcast` / `ctrl_c` / `read_panel` / `spawn_role` / `kill_panel` / `list_panels` / `register_round` / `read_menu` / `select_option`
- 레이아웃 제어 — `Ctrl-A` 프리픽스 키맵, 4종 레이아웃 모드 순환, transcript 오버레이 + 스크롤백 페이저(§9.2)
- 세션 영속화 & resume — `session.json` 레코드, `caucus sessions` / `caucus resume`(§3.1)
- agent 백엔드 다중화 — `claude` / `codex` / `gemini`, role별 `model`·`agent_cli` override, main worker 자체 판단 지정
- Claude `Stop` hook → caucus 소켓 (턴 완료 라이브 신호)
- 라이브화된 라운드 진행
- 병렬 sub-agent worktree per agent
- AgentManifest (이벤트 소싱, 유지)
- 기본 role 8종 (`main` 오케스트레이터 + 기본 sub-agent `worker` + specialist 힌트 6종: architect / backend / reviewer / qa / scribe / serious-reviewer)
- 작업 분해·병합 정책: main worker 판단

### Non-goals (v1, v2 어디서도 안 만듦)
- **범용 멀티플렉서 경쟁.** caucus는 agent 팀 전용 — role·턴 경계·main worker 오케스트레이션을 안다. 사람이 직접 쓰는 일반 터미널 멀티플렉싱은 `tmux` / `zellij`가 그 자리를 차지하고 있다.
- **zellij 크레이트 통째 vendor / fork.** grid가 zellij 내부 ~140k LOC와 결합되어 깨끗한 추출이 불가능. caucus는 zellij가 서 있는 공개 크레이트(`vte`·`portable-pty`)를 직접 쓰고 zellij는 설계 레퍼런스로만 본다.
- **LLM judge 합의.** 결과 병합·판단은 main worker Claude가 자기 컨텍스트에서 직접 함. 별도 judge agent가 들어오면 가짜 자율성이 생기고 결과 추적이 흐려짐.
- **caucus 코어의 외부 API 호출.** Notion / kodex 등 외부 동기화는 main worker가 자기 MCP 툴박스로 처리. caucus 코어는 호출하지 않는다.
- **자체 swarm / agent marketplace.** 98개 agent / 자기학습 swarm 같은 표면은 의도적으로 안 만듦.

### caucus가 명시적으로 *아닌* 것
- **Claude Code의 대체가 아님.** agent 한 명을 위한 도구가 아니라 여러 agent를 팀으로 엮는 프레임.
- **범용 멀티플렉서(tmux / zellij)의 대체가 아님.** 일반 터미널 멀티플렉싱이 필요하면 그쪽을 그대로 씀.
- **dmux의 대체가 아님.** dmux의 "사람이 멀티 agent 병렬 운영" 모델이 필요하면 dmux 그대로 씀.

