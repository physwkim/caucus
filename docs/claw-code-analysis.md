# claw-code 정독 노트 — caucus 설계 반영용

대상: `~/codes/claw-code` (ultraworkers/claw-code, Rust port of claude-code).
목적: claude-code의 subagent / agent-team 설계를 직접 코드 레벨에서 확인하고 caucus 설계에 반영.

본 노트는 임시 위치(`~/codes/`)에 두고, 새 레포 부트스트랩 시 `docs/claw-code-analysis.md`로 이동.

---

## 1. PHILOSOPHY.md가 캐묻는 세 가지 레이어

claw-code 저자가 명시적으로 분리한 3-layer 모델 — 우리 caucus 설계가 정확히 같은 분할을 따라야 함:

| Layer | claw-code 명칭 | 역할 | caucus 매핑 |
|---|---|---|---|
| 1 | **OmX** (oh-my-codex) | 워크플로 — planning, 실행 모드, verification loop, 병렬 multi-agent | `round/`, `meeting/` 모듈 |
| 2 | **clawhip** | 이벤트·알림 라우터 — git/tmux/GH/agent lifecycle 감시, **agent context window 밖에서** 알림 처리 | `hook/`, `notify/`, sentinel watcher |
| 3 | **OmO** (oh-my-openagent) | 멀티 agent 협조 — handoff, **Architect/Executor/Reviewer 의견 불일치 수렴**, verification | `consensus/`, `role/` |

핵심 문장 (PHILOSOPHY.md:50):
> Its job is to keep monitoring and delivery **outside** the coding agent's context window so the agents can stay focused on implementation instead of status formatting and notification routing.

**caucus 적용**: agent는 자기 작업 산출물만 만들고, 진행 상태·실패 분류·라우팅·Notion 업데이트는 전부 오케스트레이터(CEO + caucus 바이너리)가 담당. agent에게 "상태 한 줄 적어줘" 같은 요청을 보내면 안 됨 — 그건 컨텍스트 오염.

---

## 2. teammateMode = ["tmux", "in-process", "auto"]

`rust/crates/tools/src/lib.rs:5758` — claw-code는 멀티 agent 실행 모드를 **runtime config**로 선택:

```rust
"teammateMode" => ConfigSettingSpec {
    scope: ConfigScope::Global,
    kind: ConfigKind::String,
    path: &["teammateMode"],
    options: Some(&["tmux", "in-process", "auto"]),
},
```

코드베이스 안에서 **tmux 모드 실제 wiring은 (현재) 보이지 않고** in-process만 구현되어 있음 — config는 미래 설계 의도를 노출. 우리(caucus)는 **반대로** tmux 모드부터 구현하고 in-process 모드는 후순위.

| 모드 | 격리 | 가시성 | spawn 비용 | 워크트리 매핑 | MCP 어택서피스 | 비고 |
|---|---|---|---|---|---|---|
| **tmux (caucus v0)** | OS 프로세스 + worktree | 사람이 직접 pane 관찰 가능 | claude CLI 부팅 (~1s) | 자연스러움 | 자식이 자기 MCP 가짐 | dmux 정신 |
| in-process | tool allowlist + 권한 정책 | 로그 파일만 | 즉시 | 외부 처리 필요 | 부모 MCP 공유 | claw-code 정신 |

**결정**: caucus도 `teammateMode = "tmux" | "in-process" | "auto"` 옵션을 처음부터 받지만, v0는 tmux만 구현. in-process는 v1+.

---

## 3. Subagent 타이핑 시스템 — 가장 직접적인 차용

`rust/crates/tools/src/lib.rs:3657` (`allowed_tools_for_subagent`).

claw-code는 subagent를 **타입(string identifier)으로 일등 시민화**하고, 각 타입마다:
1. 정해진 tool allowlist
2. 생성 시 system prompt에 타입 이름 삽입

```rust
fn allowed_tools_for_subagent(subagent_type: &str) -> BTreeSet<String> {
    let tools = match subagent_type {
        "Explore" => vec![                  // 읽기 전용
            "read_file", "glob_search", "grep_search",
            "WebFetch", "WebSearch", "ToolSearch", "Skill", "StructuredOutput",
        ],
        "Plan" => vec![                     // 계획 (TodoWrite 추가)
            "read_file", "glob_search", "grep_search",
            "WebFetch", "WebSearch", "ToolSearch", "Skill",
            "TodoWrite", "StructuredOutput", "SendUserMessage",
        ],
        "Verification" => vec![             // 테스트 (bash 추가)
            "bash", "read_file", "glob_search", "grep_search",
            "WebFetch", "WebSearch", "ToolSearch",
            "TodoWrite", "StructuredOutput", "SendUserMessage", "PowerShell",
        ],
        "claw-guide" => vec![ ... ],        // claw-code 자체 도움말 특화
        "statusline-setup" => vec![ ... ],  // statusline 편집 특화
        _ => vec![ ... ],                   // general-purpose (full access)
    };
    tools.into_iter().map(str::to_string).collect()
}
```

### 3.1 caucus role 분류 — claw-code 패턴 차용

| caucus role | 모방 대상 | Tool allowlist (Claude CLI `--allowed-tools`) |
|---|---|---|
| **architect** | Plan | Read, Glob, Grep, WebFetch, WebSearch, TodoWrite |
| **backend** | general-purpose | + Bash, Edit, Write, NotebookEdit |
| **reviewer** | Explore | Read, Glob, Grep만 (수정 불가) |
| **qa** | Verification | + Bash (테스트 실행) |
| **scribe** | (신규) | Read, Glob, Grep + Notion MCP (회의록 작성) |

각 role은 caucus 설정 파일에 다음 형태로 정의:

```toml
[roles.reviewer]
allowed_tools = ["Read", "Glob", "Grep"]
permission_mode = "default"
system_prompt_template = "roles/reviewer.md"
```

### 3.2 System prompt 스캐폴딩 (claw-code:3633)

```rust
fn build_agent_system_prompt(subagent_type: &str, model: &str) -> Result<Vec<String>, String> {
    let mut prompt = load_system_prompt(cwd, date, os, "unknown", model_family);
    prompt.push(format!(
        "You are a background sub-agent of type `{subagent_type}`. \
         Work only on the delegated task, use only the tools available to you, \
         do not ask the user questions, and finish with a concise result."
    ));
    Ok(prompt)
}
```

핵심 제약 4개를 그대로 차용:
- "Work only on the delegated task" — 범위 잠금
- "use only the tools available to you" — allowlist 강조
- "do not ask the user questions" — 권한 프롬프트 자체 차단 신호
- "finish with a concise result" — 길이 제약

caucus는 위에 role-specific 가이드를 append.

---

## 4. Agent Manifest — Lane Event 타임라인

`rust/crates/tools/src/lib.rs:2596-2620` 부근. 모든 spawn된 subagent는 **단일 JSON 매니페스트**로 영속화:

```rust
struct AgentOutput {
    agent_id: String,
    name: String,
    description: String,
    subagent_type: Option<String>,
    model: Option<String>,
    status: String,                    // "running" | "completed" | "failed"
    output_file: String,               // {agent_id}.md (사람 읽기 좋은 로그)
    manifest_file: String,             // {agent_id}.json (위 구조)
    created_at: String,                // ISO8601
    started_at: Option<String>,
    completed_at: Option<String>,
    lane_events: Vec<LaneEvent>,       // 타임라인 (started/blocked/failed/finished/commit_created)
    current_blocker: Option<LaneEventBlocker>,
    derived_state: String,             // 8-state machine 결과
    error: Option<String>,
}
```

### 4.1 LaneEvent 타임라인

```rust
LaneEvent::started(ts)
LaneEvent::blocked(ts, &blocker)
LaneEvent::failed(ts, &blocker)
LaneEvent::finished(ts, detail).with_data(summary)
LaneEvent::commit_created(ts, label, provenance)
```

각 이벤트는 시간순으로 append. **이게 Notion sync의 자연스러운 단위**:
- agent가 시작 → Notion 페이지에 "started at HH:MM:SS" append
- blocker 발생 → 빨간색 callout
- commit_created → 커밋 해시 + 브랜치 + worktree 링크

### 4.2 derive_agent_state — 8-state machine

`rust/crates/tools/src/lib.rs:4361`. `(status, result, error, blocker)` 4-tuple에서 high-level state 추출:

| derived_state | 트리거 조건 |
|---|---|
| `working` | status=running |
| `finished_cleanable` | status=completed AND result 있음 |
| `finished_pending_report` | status=completed AND result 비어있음 |
| `blocked_background_job` | error에 "background" |
| `blocked_merge_conflict` | error에 "merge conflict" 또는 "cherry-pick" |
| `degraded_mcp` | error에 "mcp" |
| `interrupted_transport` | error에 "transport"/"broken pipe"/"connection"/"interrupted" |
| `truly_idle` | 그 외 |

**caucus 적용**: dmux의 `idle/working/waiting/analyzing` 4-state보다 훨씬 풍부. CEO가 retry 전략을 결정할 때 정확한 신호. 예:

- `blocked_merge_conflict` → 사람 개입 알림
- `degraded_mcp` → MCP 재시작 후 재시도
- `interrupted_transport` → backoff 후 자동 재시도
- `blocked_background_job` → background job 정리 후 재시도

각 state에 retry policy를 매핑하는 게 자연스러움.

### 4.3 commit_provenance 자동 추출

`rust/crates/tools/src/lib.rs:4402` (`maybe_commit_provenance`):

```rust
fn extract_commit_sha(result: &str) -> Option<String> {
    result.split(|c: char| !c.is_ascii_hexdigit())
        .find(|token| token.len() >= 7 && token.len() <= 40)
        .map(str::to_string)
}
```

agent 응답에서 7~40자 hex 토큰을 commit SHA로 추정. `git rev-parse --abbrev-ref HEAD` + worktree path와 묶어 `LaneCommitProvenance` 생성. `superseded_by`, `lineage` 필드까지 있어서 amend/rebase 추적 가능.

**caucus 적용**: 실행 단계 끝날 때 agent 응답에서 자동 SHA 추출 → Notion에 PR/커밋 링크 자동 첨부. 거의 무료로 얻을 수 있는 기능.

---

## 5. Spawn 모델 — std::thread (in-process)

`rust/crates/tools/src/lib.rs:3575` (`spawn_agent_job`):

```rust
fn spawn_agent_job(job: AgentJob) -> Result<(), String> {
    let thread_name = format!("clawd-agent-{}", job.manifest.agent_id);
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let result = std::panic::catch_unwind(
                std::panic::AssertUnwindSafe(|| run_agent_job(&job))
            );
            // 패닉/실패 시 manifest를 "failed" 상태로 영속화
        })
}
```

핵심: **tokio가 아니라 std::thread + catch_unwind**. agent loop가 패닉해도 부모는 살아남음 → manifest 갱신 후 다음 job 처리. 이건 in-process 모드용. caucus의 tmux 모드는 OS 프로세스 격리라 자동.

---

## 6. SubagentToolExecutor — 권한 양층 방어

`rust/crates/tools/src/lib.rs:4747`:

```rust
impl ToolExecutor for SubagentToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if !self.allowed_tools.contains(tool_name) {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled for this sub-agent"
            )));
        }
        let value = serde_json::from_str(input).map_err(...)?;
        execute_tool_with_enforcer(self.enforcer.as_ref(), tool_name, &value)
            .map_err(ToolError::new)
    }
}
```

**양층 방어**:
1. **allowlist** — 허용된 tool 이름이 아니면 dispatch 전 거절
2. **enforcer** — 허용된 tool도 `PermissionEnforcer`가 input 내용에 따라 거절 (e.g., `bash` 자체는 허용해도 `rm -rf /` 실행은 차단)

caucus의 tmux 모드에서는 `claude --permission-mode` + `--allowed-tools`로 자식 CLI에 떠넘김 (자체 enforcer 안 만듦). in-process 모드 도입 시 이 패턴 차용.

---

## 7. caucus에 그대로 차용할 결정 사항

| claw-code 패턴 | caucus 차용 형태 | 우선순위 |
|---|---|---|
| 3-layer 분리 (워크플로/이벤트/협조) | `round/`, `hook/`, `consensus/` 모듈 | ★★★ |
| **agent context window 밖에서 라우팅** | scribe role + Notion sync는 CEO·caucus만, agent 없음 | ★★★ |
| `teammateMode = tmux\|in-process\|auto` | 동일 옵션, v0 tmux만 구현 | ★★★ |
| Role = subagent type + allowlist + sys-prompt | `[roles.X]` 설정 + `--allowed-tools` 주입 | ★★★ |
| Subagent system prompt 4-제약 (delegated task / only tools / no questions / concise) | 그대로 차용 | ★★★ |
| 8-state derived_state machine | 동일 구조 채택 | ★★★ |
| LaneEvent 타임라인 | `agent_events` 테이블 + Notion append 단위 | ★★ |
| `(status, result, error, blocker)` → derived_state | 매핑 함수 동일 | ★★ |
| commit_provenance 자동 추출 | `extract_commit_sha` 동일 휴리스틱 | ★★ |
| AgentOutput 매니페스트 ({id}.md + {id}.json) | `.caucus/agents/{id}.{md,json}` | ★★ |
| SubagentToolExecutor 양층 방어 | tmux 모드는 미사용(자식 CLI에 위임), in-process 도입 시 차용 | ★ |
| std::thread + catch_unwind (in-process) | in-process 모드 도입 시 차용 | ★ |

---

## 8. 설계상 충돌 — 어느 쪽 따를지

| 차원 | dmux | claw-code | caucus 결정 |
|---|---|---|---|
| Agent 격리 | OS 프로세스 + worktree | tool allowlist + permission | **dmux 우선**(visibility), claw-code 보조(in-process 옵션) |
| Settle 감지 | screen-watch + LLM judge | spawn한 thread join으로 자연 종료 | tmux 모드 = dmux 패턴 + sentinel; in-process 모드 = thread join |
| State 표현 | 4-state (idle/working/waiting/analyzing) | 8-state derived | **claw-code 풍부함 채택** |
| 이벤트 모델 | EventEmitter (TS) | LaneEvent timeline 영속화 | **claw-code 영속화 채택** (Notion sync 자연스러움) |
| Tool 권한 | 없음 (subprocess에 위임) | 양층 (allowlist + enforcer) | tmux 모드 = subprocess 위임; in-process = 양층 |
| Agent context 격리 | tmux pane 자체가 격리 | 별도 ConversationRuntime + Session | tmux 모드는 자동 격리; in-process는 ConversationRuntime 패턴 |

---

## 9. caucus 모듈 트리 (claw-code 반영 후 갱신안)

```
caucus/
├── Cargo.toml
└── src/
    ├── tmux/                  ← dmux TmuxService 패턴
    │   ├── service.rs             spawn/send-keys/capture/kill (sendShellCommand vs sendTmuxKeys 분리)
    │   ├── status.rs              settle 감지 (sentinel 우선 + regex fallback)
    │   └── hook.rs                tmux hook → sentinel file IPC
    ├── worktree/              ← dmux WorktreeCleanup 패턴
    ├── role/                  ← claw-code subagent typing
    │   ├── registry.rs            role 정의 로드
    │   ├── allowlist.rs           role → allowed_tools 매핑
    │   └── prompt.rs              role → system prompt scaffold
    ├── agent/
    │   ├── spawn.rs               teammateMode에 따라 tmux 또는 in-process
    │   ├── manifest.rs            AgentOutput 구조체 + .md + .json 영속화
    │   ├── lane_event.rs          LaneEvent 타임라인
    │   └── derive_state.rs        8-state machine
    ├── round/                 ← OmX 대응
    │   ├── lifecycle.rs           안건 → 응답 수집 → 합의 → 실행
    │   └── transcript.rs
    ├── consensus/             ← OmO 대응
    │   ├── policy.rs              rule-based (majority/unanimous)
    │   └── judge.rs               LLM judge (CEO에 위임, MCP)
    ├── hook/                  ← clawhip 대응
    │   ├── sentinel.rs            notify(inotify/fsevents) 감시
    │   └── router.rs              이벤트 → CEO 통지 채널
    ├── ceo/                   ← 메인 오케스트레이터
    │   └── orchestrator.rs
    └── cli.rs
```

**제외 (claw-code/dmux에 있지만 caucus에서 안 만듦)**:
- Notion HTTP 클라이언트 (CEO가 MCP로 처리)
- kodex 직접 호출 (CEO가 MCP로 처리)
- ANSI 풀 파서 (TerminalDiffer 대응)
- TUI (v0 미정)

---

## 10. design.md에서 확정할 항목 (갱신)

1. **이름·경로**: `caucus` 확정. `~/codes/caucus`.
2. **teammateMode 기본값**: `tmux`. `auto`는 v1+ (감지 로직 필요).
3. **CEO 위치**: dmux 패턴 — 사용자의 메인 claude 세션이 CEO, caucus CLI를 shell로 호출.
4. **Sentinel 포맷**: `.caucus/<session>/<agent_id>.sentinel.json`
   ```json
   {
     "agent_id": "uuid",
     "ts": "2026-05-12T14:23:00Z",
     "kind": "stop" | "tool_blocked" | "error",
     "last_message": "...",
     "exit_state": "finished_cleanable" | ...
   }
   ```
5. **회의 vs 실행 단계 격리**: 회의는 worktree 없음(read-only); 실행은 worktree per agent. role-specific allowlist는 두 단계 모두 적용.
6. **합의 실패 처리**: (a) CEO 결단 기본, (b) `--escalate-on-disagree` 플래그면 사람에게, (c) `--explore-on-disagree` 플래그면 옵션별 worktree.
7. **Role 정의 위치**: `~/.caucus/roles.toml` (전역) + `<repo>/.caucus/roles.toml` (프로젝트 오버라이드).
8. **Manifest 위치**: `<repo>/.caucus/agents/<agent_id>.{md,json}` — repo-local로 git ignore 권장.

이 8개를 Phase 2 design.md 첫 섹션에 명시.
