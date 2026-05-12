# caucus — design.md

> 신규 별도 레포. Rust로 구현. dmux + claw-code 패턴을 합성한 협업 swarm 오케스트레이터.
> 본 문서는 임시 위치(`~/codes/caucus-design.md`)에 두고, Phase 1 부트스트랩 시 `<repo>/docs/design.md`로 이동.

## 0. 결정 사항 (locked)

| # | 결정 |
|---|---|
| 1 | 이름: `caucus`. 경로: `~/codes/caucus`. crates.io 미등록 확인. |
| 2 | 실행 모델 = **tmux 전용**. agent는 항상 별도 tmux pane의 `claude` CLI 프로세스. in-process / auto 같은 모드는 채택하지 않음 (§13 non-goals). |
| 3 | CEO = 사용자의 메인 claude 세션. CEO가 caucus CLI를 shell로 호출. |
| 4 | Sentinel 포맷 = `.caucus/<session_id>/agents/<agent_id>.sentinel.json`, fields: `{agent_id, ts, kind: stop \| tool_blocked \| error, last_message, exit_state}`. Claude `Stop` hook이 작성. |
| 5 | 회의 단계 = read-only (worktree 없음). 실행 단계 = worktree per agent. |
| 6 | 합의 실패 정책: 기본 (a) CEO 결단. 옵션 `--escalate-on-disagree`(사람 개입), `--explore-on-disagree`(옵션별 worktree 비교). |
| 7 | Role 정의: `~/.caucus/roles.toml` (전역) + `<repo>/.caucus/roles.toml` (프로젝트 오버라이드, 우선). |
| 8 | Agent manifest 위치: `<repo>/.caucus/sessions/<session_id>/agents/<agent_id>.{md,json}`. `.gitignore`에 `.caucus/` 추가 권장. |

본 결정은 v0에서는 변경 금지. v1 이후 재논의.

---

## 1. 시스템 다이어그램

```
사용자의 main claude 세션 = CEO
        │
        │ shell 호출 (caucus CLI)
        ▼
caucus 바이너리 ──────── tmux send-keys / split-window / kill-pane
        │                    │
        │                    ├─► role:architect pane (claude --allowed-tools ...)
        │                    ├─► role:backend pane
        │                    ├─► role:reviewer pane
        │                    └─► (회의 단계) read-only / (실행 단계) worktree
        │
        ├── .caucus/<session_id>/
        │   ├── session.json               (state machine)
        │   ├── transcript.md              (사람 읽기용 회의록)
        │   ├── agents/<agent_id>.json     (LaneEvent timeline + derived_state)
        │   ├── agents/<agent_id>.md       (사람 읽기용 agent 로그)
        │   └── agents/<agent_id>.sentinel.json  (Claude Stop hook이 작성, watcher가 감시)
        │
        └── notify → SIGUSR2 / fsevents → CEO에 깨어남 신호

CEO는 (별도로) Notion MCP·kodex MCP를 직접 호출 — caucus 바이너리는 절대 호출 안 함.
```

핵심 분리: **caucus 바이너리는 인프라**(tmux/worktree/sentinel/manifest), **CEO는 지능**(안건 작성·합의 판단·외부 sync).

---

## 2. 용어집

| 용어 | 의미 |
|---|---|
| **Session** | 한 주제의 협업 단위. 회의 → (합의) → 실행 → 리뷰의 전체 흐름. |
| **Round** | 회의의 한 라운드. 안건 → 각 role의 응답 → CEO의 정리. |
| **Role** | architect / backend / reviewer / qa / scribe 등. system prompt + tool allowlist 정의. |
| **Agent** | 한 Role의 한 인스턴스. 자기 tmux pane + (실행 단계는) 자기 worktree. |
| **CEO** | 사용자의 메인 claude 세션. caucus CLI를 통해 swarm을 조종. caucus 바이너리는 CEO 신원 모름. |
| **Sentinel** | Claude `Stop` hook이 작성하는 JSON. agent가 응답을 끝냈음을 알림. |
| **Manifest** | agent의 LaneEvent 타임라인 + derived_state + commit_provenance 영속화. |
| **Transcript** | 사람·CEO가 읽는 회의록(.md). LaneEvent의 사람 읽기용 view. |
| **Lane** | 한 agent의 작업 흐름. claw-code 용어 차용. |

---

## 3. Session 상태머신

```
                       ┌────────────────────────────┐
                       │                            │
[created] ──new──> [meeting_in_progress] ──converge──> [meeting_converged]
                       │                            │
                       │ exhaust max_rounds         │
                       ▼                            │
                  [meeting_deadlocked]              │
                       │ escalate / explore         │
                       │                            │
                       ▼                            ▼
                  [abandoned]                  [executing] ──finish──> [reviewing]
                                                    │                       │
                                                    │ blocker                │
                                                    ▼                       ▼
                                              [execution_blocked]      [merged]
                                                    │ unblock / abandon      │
                                                    ▼                       ▼
                                                 (재진입 또는 abandoned)   (terminal)
```

**불변식**: session 상태 전이는 `session::transition()` 단일 owner만 수행. 다른 모듈은 이벤트를 emit하고, 소비자가 transition을 호출.

---

## 4. Round 프로토콜

회의 단계의 한 라운드:

```
1. CEO가 작성:
     caucus round start <session_id> --agenda-file <path>
   → caucus가:
     - agenda.md를 .caucus/<session_id>/round-<n>/agenda.md로 복사
     - 각 role pane에 send_keys: "Read .caucus/<session_id>/round-<n>/agenda.md and reply to .caucus/<session_id>/round-<n>/response-<role>.md"
     - 응답 파일이 생성될 때까지 sentinel 대기

2. 각 role pane의 claude는:
     - agenda.md를 읽음
     - response-<role>.md를 작성
     - 응답 완료 후 Stop hook이 sentinel.json 작성 (kind=stop)

3. caucus watcher가 sentinel 감지 → derived_state 갱신 → CEO에게 SIGUSR2 (또는 stdout 라인)

4. CEO가:
     caucus round status <session_id>
   → caucus가 응답 수집 상태 + 각 response 경로 출력

5. 모든 응답 모이면 CEO가 판단:
     caucus round next <session_id> --agenda-file <next-agenda>   # 라운드 더
     caucus session converge <session_id> --decision-file <path>   # 합의
     caucus session deadlock <session_id>                          # 교착 — 옵션 정책으로
```

**불변식 (라운드 수 캡)**: `session.json`의 `max_rounds` (기본 5)에 도달하면 더 이상 `round next` 불가, `converge | deadlock` 둘 중 하나 강제.

---

## 5. 실행 단계

합의 후:

```
1. CEO가:
     caucus execute start <session_id> --role backend --task-file <path>
   → caucus가:
     - <repo>의 새 worktree 생성: <repo>/.caucus/worktrees/<session_id>-<role>/
     - 해당 worktree에서 새 pane spawn (claude --permission-mode acceptEdits --allowed-tools ...)
     - send_keys: "Read .caucus/<session_id>/decision.md and implement. Commit to current worktree when done."

2. agent가 commit 후 Stop hook → sentinel(kind=stop, last_message에 git log 출력 가능)

3. caucus가 sentinel에서 commit SHA 추출(extract_commit_sha 헬퍼) → 매니페스트에 commit_provenance 기록

4. CEO가:
     caucus execute finish <session_id> --role backend   # 머지 트리거
     caucus execute abandon <session_id> --role backend  # worktree 폐기
```

**worktree 정리**: `caucus execute finish/abandon` 모두 `worktree::cleanup_queue`에 enqueue. 직렬 큐. UI 안 막음.

---

## 6. Role 정의

`~/.caucus/roles.toml` (전역):

```toml
[roles.architect]
description = "Designs the approach, decomposes tasks, no code edits"
allowed_tools = ["Read", "Glob", "Grep", "WebFetch", "WebSearch", "TodoWrite"]
permission_mode = "default"      # 권한 프롬프트 안 막음. 어차피 write 도구 없음
system_prompt_template = "roles/architect.md"

[roles.backend]
description = "Implements changes. Has full file edit + bash."
allowed_tools = ["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite"]
permission_mode = "acceptEdits"  # 회의 단계엔 acceptEdits 안 의미 없음(write 안 함). 실행 단계에서 작동.
system_prompt_template = "roles/backend.md"

[roles.reviewer]
description = "Reads only. Critiques approach + code."
allowed_tools = ["Read", "Glob", "Grep", "Bash"]   # Bash는 cargo check 등을 위해
permission_mode = "default"
system_prompt_template = "roles/reviewer.md"

[roles.qa]
description = "Runs tests. Reports failures."
allowed_tools = ["Read", "Glob", "Grep", "Bash"]
permission_mode = "default"
system_prompt_template = "roles/qa.md"

[roles.scribe]
description = "Compiles final meeting transcript. No external sync."
allowed_tools = ["Read", "Glob", "Grep", "Edit", "Write"]
permission_mode = "acceptEdits"
system_prompt_template = "roles/scribe.md"
```

`<repo>/.caucus/roles.toml`로 프로젝트별 오버라이드 가능. 같은 role 이름이면 프로젝트가 우선.

### 6.1 System prompt template (claw-code 4-제약 + role-specific)

`roles/reviewer.md` 예:

```markdown
You are a `reviewer` sub-agent in a caucus session.

# Universal constraints (claw-code subagent scaffolding)
- Work only on the delegated task.
- Use only the tools available to you.
- Do not ask the user questions; if blocked, write your block reason in your response file and stop.
- Finish with a concise result.

# Role-specific
- You may NOT write or edit code. You may read, search, and (in execution phase only) run `cargo check`/`cargo test --no-run` to validate compilability.
- Your output goes to a response file in `.caucus/<session>/round-<n>/response-reviewer.md`.
- Structure your review as:
  - Findings (numbered list, file:line citations)
  - Risks (each with severity: blocker / high / medium / low)
  - Recommendation: (approve | request_changes | block)
  - Open questions for CEO
- If you find an issue, cite the anchor pattern (rg regex) so others can audit the rest of the codebase per the CLAUDE.md "Fixes from reported defects" rule.
```

`roles/architect.md`, `roles/backend.md` 등도 같은 패턴. role-specific 가이드만 다름.

---

## 7. Sentinel 프로토콜

### 7.1 파일 위치
`.caucus/<session_id>/agents/<agent_id>.sentinel.json`

### 7.2 작성 주체
**Claude `Stop` hook** (`~/.claude/settings.json` 또는 워크트리별 `.claude/settings.json`)이 작성. caucus는 이 hook을 자동 설치하지 **않음**(v0). `caucus doctor` 명령이 설치 여부 검사하고 미설치면 설치 명령을 출력.

### 7.3 hook 설치 가이드 (사람이 직접)

`~/.claude/settings.json`에 추가:

```json
{
  "hooks": {
    "Stop": [{
      "matcher": "",
      "hooks": [{
        "type": "command",
        "command": "$CLAUDE_PROJECT_DIR/.caucus/bin/sentinel-stop"
      }]
    }]
  }
}
```

`$CLAUDE_PROJECT_DIR/.caucus/bin/sentinel-stop`는 `caucus init`이 만드는 쉘 스크립트:

```bash
#!/bin/sh
# CAUCUS_SESSION_ID, CAUCUS_AGENT_ID는 caucus가 pane spawn 시 env로 주입
# CLAUDE_HOOK 환경변수(또는 stdin JSON)에서 last_message 추출
exec caucus sentinel write \
  --session "$CAUCUS_SESSION_ID" \
  --agent "$CAUCUS_AGENT_ID" \
  --kind stop
```

`caucus sentinel write` 내부:
- stdin JSON 파싱 (Claude hook이 stdin으로 전달)
- `last_message` 필드 추출
- `.caucus/<session>/agents/<agent_id>.sentinel.json` 원자적 작성 (`O_CREAT|O_EXCL` + rename)
- watcher 깨우기 (notify crate가 자동 fire)

### 7.4 sentinel JSON 스키마

```json
{
  "agent_id": "01HXY...",
  "session_id": "01HXX...",
  "ts": "2026-05-12T14:23:01Z",
  "kind": "stop",
  "last_message": "Completed reviewer pass. 3 findings, see response-reviewer.md.",
  "exit_state": null,
  "raw_hook_payload": { "...": "..." }
}
```

`kind`: `stop | tool_blocked | error`
`exit_state`: caucus가 derived_state 계산 후 채움 (`finished_cleanable` 등). hook은 null로 작성.

---

## 8. Manifest & LaneEvent

### 8.1 AgentManifest 스키마

```json
{
  "agent_id": "01HXY...",
  "session_id": "01HXX...",
  "role": "reviewer",
  "agent_name": "reviewer-r1",
  "tmux_pane_id": "%42",
  "worktree_path": null,
  "model": "opus",
  "status": "running",
  "created_at": "2026-05-12T14:20:00Z",
  "started_at": "2026-05-12T14:20:02Z",
  "completed_at": null,
  "lane_events": [
    { "kind": "started", "ts": "2026-05-12T14:20:02Z" }
  ],
  "current_blocker": null,
  "derived_state": "working",
  "error": null
}
```

### 8.2 LaneEvent 종류 (claw-code 차용 + 우리 확장)

```rust
enum LaneEventKind {
    Started,
    PromptDelivered,    // CEO가 send-keys로 안건 전달
    SentinelReceived,   // Stop hook 발사
    ResponseFileWritten,// agent가 response-*.md 작성
    Blocked { blocker: LaneEventBlocker },
    Failed   { blocker: LaneEventBlocker },
    Finished { detail: String },
    CommitCreated { provenance: LaneCommitProvenance },
    WorktreeCreated { path: PathBuf },
    WorktreeRemoved { path: PathBuf },
}
```

### 8.3 derived_state (claw-code 그대로 + caucus 확장)

```
working
finished_cleanable        (sentinel + non-empty response file)
finished_pending_report   (sentinel but empty response — 의심 신호)
blocked_background_job
blocked_merge_conflict
blocked_permission_prompt (claude CLI가 권한 프롬프트에 멈춤 — caucus 확장)
degraded_mcp
interrupted_transport
truly_idle
```

**caucus 확장 1개**: `blocked_permission_prompt` — claude가 tool 권한 프롬프트에 멈춰있을 때 감지. 다음 조건:
- sentinel 없음 AND
- pane 화면에 `Allow this tool? \[y/n\]` 류 정규식 매치

이 상태가 되면 CEO에게 알림. 자동 yes는 안 함 (위험).

### 8.4 derive 함수 (claw-code 4-tuple + caucus 신호)

```rust
fn derive_agent_state(
    status: &str,
    response_file: Option<&Path>,
    error: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
    pane_screen_hint: Option<&PaneScreenHint>,
) -> DerivedState
```

매 sentinel/sigusr2 시점에 재계산.

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
│   ├── architect.md
│   ├── backend.md
│   ├── reviewer.md
│   ├── qa.md
│   └── scribe.md
└── src/
    ├── main.rs              (clap CLI 진입점)
    ├── lib.rs               (라이브러리 노출, 테스트용)
    ├── cli.rs               (서브커맨드 dispatch)
    ├── config/
    │   ├── mod.rs           (글로벌 + 프로젝트 config 병합)
    │   └── roles.rs         (roles.toml 파싱)
    ├── session/
    │   ├── mod.rs           (Session 구조체)
    │   ├── state.rs         (상태머신, transition() 단일 owner)
    │   └── id.rs            (ULID 발급)
    ├── role/
    │   ├── registry.rs      (이름 → RoleSpec 조회)
    │   └── spec.rs          (RoleSpec 구조체: allowlist, prompt_template, permission_mode)
    ├── tmux/
    │   ├── service.rs       (spawn/send_shell/send_keys/capture/kill — dmux 패턴)
    │   ├── escape.rs        (sendShellCommand vs sendTmuxKeys 분리)
    │   └── hook.rs          (tmux hook 설치/제거, SIGUSR2 또는 sentinel 파일 IPC)
    ├── worktree/
    │   ├── manager.rs       (생성)
    │   └── cleanup.rs       (직렬 큐 + depth-desc 정렬, tokio::mpsc consumer)
    ├── agent/
    │   ├── spawn.rs         (RoleSpec → 새 tmux pane + 새 AgentManifest)
    │   ├── manifest.rs      (AgentManifest 영속화: .json + .md 페어)
    │   ├── lane_event.rs    (LaneEvent enum + append)
    │   ├── derive_state.rs  (4-tuple + pane_hint → DerivedState)
    │   └── provenance.rs    (extract_commit_sha + git rev-parse → LaneCommitProvenance)
    ├── sentinel/
    │   ├── writer.rs        (Claude Stop hook이 호출하는 `caucus sentinel write`)
    │   └── watcher.rs       (notify crate로 파일 감시, ingest → manifest 갱신)
    ├── status/
    │   ├── pane_hint.rs     (화면 regex 패턴 매칭: esc-to-interrupt, permission prompt, *ing...)
    │   └── poller.rs        (sentinel 미발사 시 fallback polling, dmux 패턴 단순화)
    ├── round/
    │   ├── lifecycle.rs     (start/next/converge/deadlock)
    │   └── transcript.rs    (round-<n>/agenda.md + response-<role>.md → transcript.md 통합)
    ├── consensus/
    │   └── policy.rs        (v0: CEO 결단만. v1+: rule-based 다수결 등)
    ├── execute/
    │   └── lifecycle.rs     (worktree 생성 + 실행 agent spawn + finish/abandon)
    ├── notify/
    │   └── signal.rs        (sigusr2 또는 fsevents로 CEO 깨우기)
    └── doctor.rs            (tmux/git/claude/hook 설치 점검)
```

### 9.1 모듈 owner 매트릭스 (불변식 enforcement)

| 자원 | 단일 owner | 규칙 |
|---|---|---|
| Session state 전이 | `session::state::transition()` | 다른 모듈은 event만 emit |
| AgentManifest 작성 | `agent::manifest::write()` | 외부 직접 write 금지 |
| Sentinel 파일 작성 | `sentinel::writer::write()` 또는 Claude hook | watcher는 read-only |
| Sentinel 파일 read | `sentinel::watcher::ingest()` | 직접 fs::read 금지 |
| tmux pane 생성 | `tmux::service::spawn_pane()` | 직접 `tmux split-window` 금지 |
| tmux pane 종료 | `tmux::service::kill_pane()` | 직접 `tmux kill-pane` 금지 |
| worktree 생성 | `worktree::manager::create()` | 직접 `git worktree add` 금지 |
| worktree 삭제 | `worktree::cleanup::enqueue()` | 직접 삭제 금지. 직렬 큐를 통해서만 |
| Notion / kodex 호출 | **caucus 안 함** | CEO만 MCP로 호출 |

각 모듈은 외부에 노출하는 함수 외엔 `pub(crate)` 미만으로 잠금. Rust visibility로 강제.

---

## 10. CLI surface (v0)

```
caucus init                              # .caucus/ 디렉터리 + bin/sentinel-stop 생성, .gitignore 안내
caucus doctor                            # tmux/git/claude/hook 점검

caucus session new --topic "..." --roles architect,backend,reviewer
                                         # 새 session, role마다 tmux pane spawn (회의 단계 = read-only)
caucus session list
caucus session show <session_id>         # state + 각 agent의 derived_state + 최근 LaneEvent
caucus session kill <session_id>         # 모든 pane kill, cleanup queue enqueue

caucus round start <session_id> --agenda-file <path>
                                         # 모든 role pane에 안건 전달, 응답 수집 시작
caucus round status <session_id>         # 각 role의 응답 상태 (대기/완료/blocked)
caucus round next <session_id> --agenda-file <path>
                                         # 다음 라운드 (이전 round-N를 컨텍스트로 포함)

caucus session converge <session_id> --decision-file <path>
                                         # 합의 단계로 전이. decision.md를 transcript에 lock.
caucus session deadlock <session_id>     # 교착 처리. --escalate / --explore 옵션

caucus execute start <session_id> --role <role> --task-file <path>
                                         # 새 worktree + 실행 agent spawn
caucus execute status <session_id>
caucus execute finish <session_id> --role <role>   # 머지 시도 + worktree 정리 enqueue
caucus execute abandon <session_id> --role <role>  # worktree 정리 enqueue

caucus agent show <agent_id>             # manifest JSON 출력
caucus agent send <agent_id> "msg"       # ad-hoc send-keys (긴급 개입용)
caucus agent kill <agent_id>             # 단일 agent kill

caucus role list                         # 사용 가능한 role
caucus role show <name>                  # spec + prompt template 전체

caucus sentinel write --session <id> --agent <id> --kind stop
                                         # Claude Stop hook이 호출 (사람은 안 침)

caucus watch <session_id>                # foreground watcher (CEO가 이걸 백그라운드로 띄움)
                                         # stdout으로 이벤트 라인 emit, CEO가 tee로 수신
```

### 10.1 exit code 규약

- `0` — 성공
- `2` — 사용자 오류 (잘못된 인자, 미존재 session 등)
- `3` — 환경 오류 (tmux 없음, git 없음, claude CLI 없음)
- `4` — caucus 상태 비정상 (manifest 손상 등) — `caucus doctor` 권유
- `1` — 예상 못한 실패 (panic 등). bug 로 간주.

CEO가 exit code 보고 자동 분기.

### 10.2 stdout / stderr 분리

- 정형 데이터(상태, 매니페스트) → stdout, JSON
- 사람 읽기 메시지·진행상황 → stderr, 텍스트
- `--format json|text` 플래그로 stdout 포맷 선택

CEO는 거의 항상 `--format json`을 씀.

---

## 11. CEO 워크플로 (실제 사용 시나리오)

사용자가 `caucus` 자체를 직접 안 쓰고, CEO가 자기 손으로 호출하는 흐름. 사용자는 CEO에게 자연어로 명령.

### 시나리오: "epics-archiver의 write_loop를 합의 후 리팩토링하자"

```text
사용자 → CEO: "epics-archiver의 write_loop 다시 짜자. 회의 한 번 하고 결정 나면 backend가 구현, reviewer가 검토하는 식으로."

CEO → shell:
  cd ~/codes/archiver-rs
  caucus session new \
    --topic "write_loop refactor (epics-archiver)" \
    --roles architect,backend,reviewer \
    --format json
  # → 응답: {"session_id": "01HXX...", "panes": [...]}

CEO → shell:
  cat > /tmp/agenda.md << 'EOF'
  # 안건: write_loop 리팩토링

  현재 epics-archiver write_loop는 ... (CEO가 컨텍스트 추가).

  각 role:
  - architect: 문제 진단 + 옵션 2~3개 제시
  - backend: 각 옵션의 구현 난이도·위험·기대 효과 평가
  - reviewer: 각 옵션의 위험·invariant·테스트 가능성 검토

  응답은 마크다운, 각 섹션은 250단어 이내.
  EOF

  caucus round start 01HXX --agenda-file /tmp/agenda.md --format json

  # CEO는 백그라운드 watcher 띄워둠
  caucus watch 01HXX > /tmp/caucus.events &

CEO → 주기적으로:
  caucus round status 01HXX --format json
  # → {"round": 1, "responses": {"architect": "ready", "backend": "ready", "reviewer": "pending"}}

# reviewer까지 ready 되면:
CEO → 응답 3개를 직접 읽고 종합 판단. (CEO가 Claude니까 잘 함)
  cat .caucus/01HXX/round-1/response-*.md

# CEO 판단: 옵션 B로 가야 함, 다만 reviewer가 제기한 X invariant는 명시 필요.
CEO → 차기 라운드 안건 작성:
  cat > /tmp/agenda-r2.md << 'EOF'
  # Round 2: 옵션 B 구체화

  옵션 B(write coalescing)로 결정. 단 reviewer가 제기한 X invariant 보존 필수.

  - architect: 옵션 B의 모듈 경계 + invariant enforcement 메커니즘 1쪽 안.
  - backend: 옵션 B의 PR 분할 계획 (어떤 커밋 N개로 나눌지).
  - reviewer: X invariant가 깨질 수 있는 코너 케이스 enumeration.
  EOF
  caucus round next 01HXX --agenda-file /tmp/agenda-r2.md

# (몇 라운드 후) 합의 도달
CEO →:
  cat > /tmp/decision.md << 'EOF'
  # 결정

  - 옵션: B (write coalescing)
  - PR 분할: 3개 (P1 = 채널 큐 도입, P2 = coalescer 트레이트, P3 = 기존 호출부 마이그레이션)
  - X invariant 보존 메커니즘: ...
  - 첫 PR(P1)을 backend가 구현, reviewer가 검토. 통과 후 P2.
  EOF
  caucus session converge 01HXX --decision-file /tmp/decision.md

# 실행 단계 — P1 구현
CEO →:
  caucus execute start 01HXX --role backend \
    --task-file /tmp/decision.md
  # → 새 worktree + agent. backend가 코드 작성 + 커밋.

  caucus execute status 01HXX --format json
  # → {"backend": {"derived_state": "finished_cleanable", "commit_provenance": {...}}}

# backend가 commit_created 받으면 reviewer 실행 단계:
CEO →:
  caucus execute start 01HXX --role reviewer \
    --task-file /tmp/decision.md  # reviewer는 worktree만 다름

  # reviewer 끝나면:
  cat .caucus/01HXX/execute/reviewer/response.md
  # CEO가 읽고 판단.

# 통과:
CEO →:
  caucus execute finish 01HXX --role backend  # 머지 시도
  caucus execute finish 01HXX --role reviewer # worktree 정리

# CEO는 Notion MCP 직접 호출해 회의록 동기화:
CEO → (자기 자신의 도구):
  mcp__notion__notion-update-page(...transcript.md 내용...)
CEO → (kodex MCP):
  mcp__kodex__learn(title="...", description="...")
```

이 흐름이 v0 MVP의 정확한 사용 패턴. **caucus 바이너리는 Notion/kodex를 모름.** CEO Claude가 자기 MCP 툴박스로 처리.

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

### Invariant I-4: caucus 바이너리는 Notion / kodex 호출 금지
- **Owner**: (없음 — 부재가 invariant)
- **MUST**: caucus crate의 `Cargo.toml`에 `reqwest`, `tonic`, `kodex` 등 외부 sync용 dep 없음.
- **MUST NOT**: 어느 코드 경로도 외부 sync API 호출.
- **Enforcement**: `Cargo.toml` deny 리스트 + CI에서 `cargo tree | grep -E "(reqwest|tonic)"` 가 비어있는지 검사.

### Invariant I-5: Sentinel 파일은 Claude hook + caucus sentinel writer만 작성
- **Owner**: 외부(Claude hook) 또는 `sentinel::writer::write()`
- **MUST**: 두 경로 모두 atomic write (`O_CREAT|O_EXCL` + `rename`).
- **MUST NOT**: watcher가 sentinel 파일 작성. watcher는 read-only.
- **Enforcement**: watcher 모듈에 `fs::write` import 없음. lint로 검사.
- **Tests**: hook이 sentinel 작성 중일 때 watcher가 부분 파일 read 안 하는지.

---

## 13. 스코프와 non-goals

### v0 안에 들어 있는 것
- `caucus init` (+ `--install-hook`), `caucus doctor`
- `caucus session new/list/show/converge/deadlock/kill/transcript`
- `caucus session deadlock --escalate | --explore` 정책 분기
- `caucus round start/status/next`
- `caucus execute start/status/finish/abandon`
- `caucus agent list/show/send/kill`
- `caucus role list/show`
- `caucus sentinel write` + notify 기반 watcher
- `caucus watch` 포그라운드 이벤트 스트림 (heartbeat + SIGINT + SIGUSR2)
- AgentManifest + LaneEvent + 8-state derived_state + commit_provenance
- 회의 단계 read-only / 실행 단계 worktree per role
- 5종 role (architect / backend / reviewer / qa / scribe) + role 별 `model` override
- 합의 정책: CEO 결단

### Non-goals (v1, v2 어디서도 안 만듦)
- **in-process 실행 모델.** caucus는 tmux 전용. 다른 형태로 agent를 띄우는 모드(같은 프로세스의 worker thread, fork, container 등)는 채택하지 않음. 격리·관찰성·기존 dmux 호환성을 위해 OS 프로세스 + 별도 pane이라는 단일 모델만 유지.
- **`teammateMode` 같은 실행 모델 선택기.** 모드가 하나뿐이라서 선택지 자체가 불필요.
- **LLM judge 합의.** 합의 판단은 CEO Claude가 자기 컨텍스트에서 직접 함. 별도 judge agent가 들어오면 가짜 자율성이 생기고 결과 추적이 흐려짐.
- **TUI (ratatui 등).** caucus는 CEO Claude가 호출하는 인프라 CLI. 사람 손으로 직접 조작하는 도구가 필요하면 `dmux`가 그 자리를 차지하고 있음.
- **자체 swarm/agent marketplace.** 98개 agent / 자기학습 swarm 같은 표면은 의도적으로 안 만듦.

### 차후 확장 후보 (caucus가 직접 안 만들고 별도 레포 / plugin으로 갈 수 있는 항목)
- claude `Stop` hook 자동 설치는 이미 `caucus init --install-hook`으로 제공됨.
- `--escalate-on-deadlock` Discord/Slack webhook 어댑터 (현재 `escalated.signal` 파일을 외부 watcher가 읽도록 위임).
- Notion / kodex MCP 어댑터 — CEO가 자기 MCP로 처리하면 되므로 caucus 코어에 추가하지 않음.
- 외부 관찰자용 web view — 별도 컨슈머가 `.caucus/` 디렉터리를 직접 읽으면 충분.

### caucus가 명시적으로 *아닌* 것
- **Claude Code의 대체가 아님.** agent 한 명을 위한 도구가 아니라 여러 agent의 협업.
- **dmux의 대체가 아님.** dmux의 "사람이 멀티 agent 운영" 모델이 필요하면 dmux 그대로 씀.
- **ruflo의 대체가 아님.** 거대 swarm 플랫폼이 필요하면 ruflo로 가는 게 맞음.

---

## 14. 보안·신뢰 가정

- 같은 호스트의 같은 사용자가 모든 agent를 띄움. 멀티 테넌시 가정 없음.
- claude CLI가 신뢰 가능. caucus는 claude를 sandbox하지 않음.
- `acceptEdits` 모드 사용 시 backend role이 임의 파일 편집·bash 실행 가능. **위험한 명령(`rm -rf`, `git push --force` 등)은 reviewer role이 사전에 안건에서 차단해야 함.** caucus가 막아주지 않음.
- sentinel 파일은 같은 사용자 권한으로만 작성/읽기. cross-user 공격 표면 없음.

---

## 15. 열린 질문 (v0 진행 중에 결정)

| 질문 | 후보 | 결정 시점 |
|---|---|---|
| 라운드 안건 전달 방식 — send-keys로 직접 prompt 입력? 아니면 파일 경로만 알려주고 read 시킴? | 후자 (파일 경로 + read 지시) — 컨텍스트 윈도우 낭비 적음 | 1차 round 구현 후 검증 |
| 응답 파일을 agent가 write 안 했는데 sentinel 떴을 때 | `finished_pending_report` derived_state로 신호 | 첫 사용 후 |
| 여러 round 누적 컨텍스트 — agent가 매 라운드 새 prompt? 아니면 같은 claude 세션 유지? | 같은 pane = 같은 claude 세션 유지 (자연스러움) | 결정됨 |
| pane 종료 후 worktree 보존 기간 | execute finish 시 즉시 enqueue, abandon은 24h 유예? | v0 simplicity: 즉시 enqueue |
| sentinel watcher가 죽으면 | foreground `caucus watch`로 띄우고 죽으면 CEO가 재기동. v1+에서 daemon 모드 | 결정됨 |
| max_rounds 기본값 | 5. 사용자 override 가능. | 결정됨 |

---

## 16. 다음 단계 (Phase 1 — 부트스트랩)

이 design.md가 OK되면 Phase 1 시작. 구체 작업:

1. `cargo new ~/codes/caucus`
2. `Cargo.toml`에 deps: tokio, clap, serde, serde_json, anyhow, thiserror, tracing, tracing-subscriber, uuid (v1, v7), ulid, notify, regex, once_cell, sha2 (또는 xxhash-rust)
3. 모듈 트리 (§9) — 빈 `mod.rs` + 핵심 struct/trait 시그니처만
4. `docs/design.md` 이동, `docs/dmux-analysis.md`, `docs/claw-code-analysis.md` 이동
5. `roles/` 디렉터리 + 5종 prompt 템플릿 초안
6. `.gitignore` (`.caucus/` 포함)
7. `README.md` (1-2단락)
8. `cargo check` 통과 확인
9. 첫 커밋 (`git init` 포함)

코드는 안 짜고 골격만. 사용자가 보고 `다음`이라고 하면 v0 구현 작업 시작.
