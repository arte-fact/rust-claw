# rust-claw Architecture

A Rust reimagining of [NanoClaw](https://github.com/nanocoai/nanoclaw): a personal AI assistant where a
single daemon routes messages to agent sessions, runs the agent, and delivers replies. NanoClaw's core
ideas are kept — the entity model, message-table scheduling, "everything is a message" — but the
deployment model is inverted: **NanoClaw runs agents in containers next to a bare-metal host;
rust-claw runs the whole app in one container and agents as plain child processes inside it.**

```
web UI (built-in) ──┐
future adapters ────┼──▶ claw (router) ──▶ session.db ──▶ native agent loop (in-process) ──▶ session.db ──▶ claw (delivery) ──▶ web UI / adapters
                    │         one process, one container, one /data volume
```

Three deliberate departures from upstream, decided up front:

1. **Built-in web interface** is the primary channel (no Telegram/WhatsApp adapter in trunk).
2. **No per-agent Docker.** The app itself is dockerized; agent runs are **strictly sequential**
   (global FIFO queue, one at a time) on a shared persistent filesystem.
3. **The agent is home-made and in-process**: a native chat-completion loop against any
   OpenAI-compatible endpoint, with messaging tools plus `bash`/`read`/`write`/`edit` for coding
   groups. No external agent harness — see
   [docs/decisions/001-drop-pi.md](docs/decisions/001-drop-pi.md) for why pi was built then removed.

---

## 1. Goals and non-goals

**Goals**

- Small enough to understand: **one crate, one binary** (`claw`), 100 % Rust.
- Same mental model as NanoClaw: users → messaging groups → agent groups → sessions; all IO between
  router and agent is rows in a session SQLite DB; scheduling is `process_after`/`recurrence` on those rows.
- Self-contained deployment: `docker compose up`, one volume at `/data`, web UI on one port.
- Idiomatic Rust: typed ids, channels over callbacks, pure decision functions, `Result` everywhere.
- Customization = code changes. Fork, edit, `cargo build`. No config sprawl.

**Non-goals**

- Inter-agent security isolation. Agents share the app's filesystem and can read anything in the
  container. The Docker boundary protects the *host machine*, not sessions from each other (§11).
- Concurrent agent runs. Sequential is a feature: predictable load, no warm-pool logic, no per-claim
  stuck arbitration.
- Platform messenger adapters in trunk. The `ChannelAdapter` trait exists and the web channel implements
  it; Telegram/Discord/etc. are fork work.

---

## 2. Deployment model

One image, one container, one volume:

```
compose.yaml              # claw service, port 8080, volume claw-data:/data
Dockerfile                # multi-stage: rust:1.96 builder (build-essential for rusqlite's
                          # bundled SQLite) → debian-slim + /claw binary + CA certs + bash/git/curl
                          # (the coder tool surface). No Node — one binary (decision 001).
scripts/smoke.sh          # build + boot the image, assert healthz + the auth gate
/data/                    # the persistent world — everything lives here
  central.db              # entities, routing, web message ledger
  auth_token              # generated login token (0600), reused across restarts
  sessions/<group>/<id>/  # session.db + inbox/ + outbox/
  groups/<folder>/        # per-agent-group filesystem: AGENT.md, working files (= agent cwd)
  logs/
  claw.sock               # admin CLI socket
```

The app is stateless outside `/data`. Upgrading = new image + same volume. Inference API keys are
plain env vars on the container or rows in the `endpoints` table (§8.7) — no credential proxy.
**First run** (M8.1): with no `CLAW_AUTH_TOKEN`, the daemon generates a token, persists it to
`/data/auth_token`, and logs it once (`docker compose logs claw`); an explicit env token always
wins and is never written to disk.

---

## 3. Process model

```
┌─ claw (tokio) ────────────────────────────────────────────────────────────┐
│                                                                           │
│  axum server ── web UI (SSE + REST) ── auth (token cookie)                │
│      │ InboundEvent                                                       │
│      ▼                                                                    │
│  router task ──writes──▶ session.db ──enqueue──▶ RunQueue (global FIFO)   │
│                                                       │ pop (one at a time)
│  delivery task (1s active / 60s sweep) ◀──reads─┐     ▼                   │
│  sweep task (60s: due schedules, recurrence,    │  run supervisor task    │
│              stuck-run watchdog)                │     ▼                   │
│  cli server (unix socket)                       │  native agent loop      │
│                                                 │  (LLM ⇄ tools; bash     │
└─────────────────────────────────────────────────┴── runs as a child) ─────┘
```

One OS process: the `claw` daemon. The only children are short-lived `bash` tool invocations made
by coding-profile agent runs. The former NanoClaw "agent-runner" is the **run supervisor** module
inside `claw` (a tokio task that drives the provider loop and manages message status); the native
agent loop writes `messages_out` itself through its tools.

**Shutdown:** `CancellationToken` everywhere; SIGTERM → stop accepting work, abort the current
agent run (its messages reset to `pending`), flush, exit 0. Crash-loop circuit breaker as in
upstream (file-based exponential backoff, cleared on clean exit).

---

## 4. Crate layout

Single crate, lib + bin. The CLI client is a subcommand of the same binary.

```
rust-claw/
  Cargo.toml
  ARCHITECTURE.md
  src/
    main.rs               # clap: `claw serve` (daemon) | `claw <resource> <verb>` (socket client)
    config.rs             # DATA_DIR, PORT, AUTH token, TIMEZONE, limits — the only shared config
    protocol/             # the contract (would-be claw-protocol crate, now a module)
      ids.rs              #   newtype ids (AgentGroupId, SessionId, UserId, …)
      message.rs          #   MessageKind, InboundContent, OutboundContent, Operation, Routing
      frame.rs            #   CLI RequestFrame / ResponseFrame / ErrorCode
      action.rs           #   system-action payloads
    db/                   # central DB: one file per entity + numbered migration runner
    session/              # session resolution, folders, session.db schema + access layer
    router.rs             # inbound routing (engage modes, gates, command gate, fan-out)
    runs/
      queue.rs            #   RunQueue: FIFO + dedup set
      supervisor.rs       #   pop → drain session → run provider → status transitions
      formatter.rs        #   pending messages → XML prompt (for prompt-consuming providers)
    providers/
      mod.rs              #   AgentProvider trait + registry
      native/             #   THE agent: chat-completion loop (OpenAI-compatible endpoints)
        client.rs         #     /chat/completions client with tool-calling
        context.rs        #     transcript → token-budgeted message window
        tools.rs          #     messaging tools (send_message, schedule_task, send_to_agent)
        exec.rs           #     bash tool (workspace cwd, timeout, output truncation)
        files.rs          #     read / write / edit (coder profile)
      echo.rs             #   no-network test provider
    mcp/                  # stdio MCP client (M12): conn.rs (newline-delimited JSON-RPC
                          # over a child's stdin/stdout) + McpClient (spawn, handshake,
                          # list/call). Tools namespaced <server>__<tool> for every agent.
    logs.rs               # in-memory ring buffer + tracing Layer (M13): the last ~1000
                          # records, broadcast to the /admin/logs live viewer; claw.log
                          # keeps the durable history
    delivery.rs           # messages_out polling, system actions, channel dispatch
    sweep.rs              # 60s: due schedules → enqueue, recurrence advance, watchdog
    channels/
      mod.rs              #   ChannelAdapter trait + build_adapters() barrel
      web.rs              #   the built-in adapter (bridges axum ⇄ router/delivery)
    web/                  # axum: routes, SSE hub, auth middleware, embedded static UI
    commands/             # admin command registry + per-resource defs (CRUD); args carry
                          # display metadata (label, type, options) so the web admin renders
                          # itself from the registry (§9.2)
    workspace/            # jailed agent-folder filesystem service for the web file
                          # browser: path.rs (canonicalizing jail — no escape, unlike
                          # providers::native::files) + ops.rs (list/read/write/…) (§11)
    cli_server.rs         # unix-socket frame server
    cli_client.rs         # socket client used by the CLI subcommands
    modules/              # optional hooks: permissions, approvals, agent-to-agent
  templates/              # Askama templates (full pages + SSE fragments — one rendering path)
  assets/                 # claw.css (Nord tokens), claw.js (~150 lines SSE/composer),
                          # FiraCodeNerdFont woff2 — embedded into the binary via rust-embed
```

---

## 5. Invariants

NanoClaw's session-DB rules were mostly VirtioFS-survival tactics. Same-kernel, local-filesystem
operation lets us drop the exotic ones and keep the architectural ones:

**Kept:**

1. **Everything is a message.** Chat, tasks, webhooks, agent commands, agent-to-agent: rows in
   `messages_in` / `messages_out`. Scheduling is `process_after` / `deliver_after` + `recurrence` on
   those rows. No separate scheduler, no RPC between subsystems.
2. **Per-table single writer per role.** The host side (router, delivery) writes `messages_in`,
   `delivered`, `session_routing`, `destinations`; the agent side (the native loop's tools, on
   behalf of the run) writes `messages_out`. The roles live in one process now, but the split
   keeps replay, delivery, and recovery reasoning simple — and keeps the door open for external
   agent processes later.
3. **Seq parity:** the host side assigns even `seq` in `messages_in`, agent runs assign odd in
   `messages_out`. One global ordering across both tables; `edit_message(5)` resolves its table
   by parity.
4. **The agent never sees routing.** `platform_id`/`channel_type`/`thread_id` are stripped before
   formatting; replies inherit routing from the row they answer, and the host validates destinations.
5. **Presentation signals stay out of the agent ledger (M14).** The chat activity indicator
   (ephemeral `run` SSE) and error cards (`web_messages` rows, the web channel's view) are additive
   UI only — never written to `messages_in`/`messages_out`. The `RunNotifier` seam is optional and
   side-effect-free w.r.t. a run: seq parity, `trigger` accumulation, and engage logic are identical
   with or without it attached.

**Dropped (and why):**

| Upstream rule | Why it existed | Why it's gone |
|---|---|---|
| Two DB files per session | cross-mount lock contention | same process tree, local FS → **one `session.db`**, WAL |
| `journal_mode=DELETE`, `mmap_size=0` | VirtioFS mmap incoherency | local FS: WAL + `busy_timeout` is correct and faster |
| host opens-writes-closes per op | stale page cache across mount | long-lived pooled connections are fine |
| `.heartbeat` file mtime | no cheap cross-mount liveness | supervisor owns the child; liveness = RPC event flow |
| `processing_ack` / `container_state` tables | container couldn't write host's DB | supervisor updates `messages_in.status` directly |
| `on_wake` column / dying-container wake race | concurrent container teardown | one sequential supervisor; no race exists |

---

## 6. Domain types (`protocol/`)

Unchanged in spirit from the first draft — newtype ids with `Display/FromStr/ToSql/FromSql`,
kebab-case enum serialization so the DB stays `sqlite3`-readable:

```rust
string_id!(AgentGroupId, MessagingGroupId, SessionId, UserId, MessageInId, MessageOutId);

pub enum MessageKind { Chat, Task, Webhook, System }          // chat-sdk dropped with the bridge
pub enum MessageStatus { Pending, Processing, Completed, Failed, Paused }
pub enum SessionMode { Shared, PerThread }                    // agent-shared deferred
pub enum EngageMode { Pattern, Mention, MentionSticky }

pub enum InboundContent { Chat(ChatContent), Task(TaskContent), Webhook(WebhookContent),
                          System(SystemResult), Raw(serde_json::Value) }

pub struct OutboundContent {
    pub text: Option<String>,
    pub files: Vec<String>,                  // outbox/<msg_id>/ convention is the contract
    pub operation: Option<Operation>,        // AskQuestion | Edit | Reaction | Card
    pub extra: serde_json::Map<String, Value>,
}
```

The web UI renders `Operation`s natively (buttons, edits, reactions) — it is the *most* capable
channel, and the degradation path in delivery only matters for future adapters.

---

## 7. Session DB (one file)

`/data/sessions/<group>/<session>/session.db`, WAL, created at session-folder init:

```sql
CREATE TABLE messages_in (
  id            TEXT PRIMARY KEY,
  seq           INTEGER UNIQUE,              -- EVEN, claw-assigned
  kind          TEXT NOT NULL,               -- chat|task|webhook|system
  timestamp     TEXT NOT NULL,
  status        TEXT DEFAULT 'pending',      -- pending|processing|completed|failed|paused
  process_after TEXT,                        -- NULL = now
  recurrence    TEXT,                        -- cron, NULL = one-shot
  series_id     TEXT,
  tries         INTEGER DEFAULT 0,
  trigger       INTEGER NOT NULL DEFAULT 1,  -- 0 = accumulate (context only, no run)
  platform_id   TEXT, channel_type TEXT, thread_id TEXT,
  content       TEXT NOT NULL,               -- JSON by kind
  source_session_id TEXT                     -- agent-to-agent return path
);

CREATE TABLE messages_out (                  -- written ONLY by agent runs (native tools)
  id TEXT PRIMARY KEY,
  seq INTEGER UNIQUE,                        -- ODD, agent-assigned
  in_reply_to TEXT, timestamp TEXT NOT NULL,
  deliver_after TEXT, recurrence TEXT,
  kind TEXT NOT NULL,
  platform_id TEXT, channel_type TEXT, thread_id TEXT,
  content TEXT NOT NULL
);

CREATE TABLE delivered (                     -- claw's delivery ledger
  message_out_id TEXT PRIMARY KEY,
  platform_message_id TEXT,
  status TEXT NOT NULL DEFAULT 'delivered',
  delivered_at TEXT NOT NULL
);

CREATE TABLE destinations (name TEXT PRIMARY KEY, display_name TEXT, type TEXT NOT NULL,
                           channel_type TEXT, platform_id TEXT, agent_group_id TEXT);

CREATE TABLE session_routing (id INTEGER PRIMARY KEY CHECK (id = 1),
                              channel_type TEXT, platform_id TEXT, thread_id TEXT);
```

There is no separate conversation store: the transcript across both tables (§8.5) *is* the
agent's memory — no continuation token to persist, rotate, or invalidate.

```
sessions/<group>/<id>/
  session.db
  inbox/<msg_id>/      ← inbound attachments (O_EXCL create, realpath containment — kept from upstream)
  outbox/<msg_id>/     ← files the agent sends (send_file moves them here)
```

Agent runs execute with `cwd = /data/groups/<folder>/` (the agent group workspace: `AGENT.md`
instructions and persistent working files; `AGENT.md` becomes the system prompt, and the bash/file
tools operate here).

---

## 8. The run pipeline

### 8.1 RunQueue — sequential by construction

```rust
pub struct RunQueue {                       // behind a Mutex, notified via tokio::sync::Notify
    queue: VecDeque<SessionId>,
    queued: HashSet<SessionId>,             // dedup: a session appears at most once
}
impl RunQueue { pub fn enqueue(&mut self, id: SessionId) -> bool { … } }
```

`wake(session)` from router, sweep, delivery, or CLI = `enqueue`. One supervisor task pops; **at most
one agent run exists at a time, globally**. There is no idle/warm state and no concurrency cap to
tune — the queue *is* the policy.

### 8.2 Run supervisor — drain and exit

```
pop session →
  loop:
    batch = messages_in WHERE status='pending' AND due, seq order        (none? exit loop)
    if all trigger=0 → leave as accumulated context, exit loop
    mark batch 'processing'
    run = provider.start(...)                                            (native: reads transcript itself)
    stream events until TurnEnd / Error
    mark batch 'completed'                                               (failed on provider error)
    → loop (messages that arrived during the run are picked up next iteration)
  touch last_active, pop next
```

- **Status transitions are direct `UPDATE`s on `messages_in`** — the supervisor and the DB are in the
  same process; no ack table, no sync step.
- **Mid-run arrivals**: the router enqueues as usual; if the running session == the arriving session,
  the supervisor's drain loop picks the rows up on the next iteration as a fresh provider turn
  (the transcript carries the continuity).
- **Watchdog** (in sweep): if a run has produced no provider events for `max(10 min, declared tool
  timeout)`, abort it; its `processing` rows get `tries += 1` and
  `process_after = now + 5s·2^tries`, `tries ≥ 5` → `failed`. Same backoff policy as upstream,
  one global subject instead of N containers.
- **Crash recovery**: on daemon startup, any `processing` row reverts to `pending` (a run cannot
  outlive the process that supervises it).

### 8.3 AgentProvider trait

```rust
pub struct QueryInput { pub prompt: String, pub cwd: PathBuf, pub session_dir: PathBuf,
                        pub model: Option<ModelRef>, pub env: Vec<(String, String)> }

pub enum ProviderEvent {
    TurnEnd { text: Option<String> },        // a completed assistant turn
    Activity,                                // any event — feeds the watchdog
    Progress { message: String },            // tool execution updates → web UI live status
    Error { message: String, retryable: bool },
}

pub trait AgentProvider: Send + Sync {
    fn start(&self, input: QueryInput) -> Result<ActiveRun, ProviderError>;
}

pub struct ActiveRun {
    pub input: mpsc::Sender<String>,         // mid-run follow-ups (provider-defined semantics)
    pub events: mpsc::Receiver<ProviderEvent>,
    pub abort: CancellationToken,            // cancels the run (and any tool children)
}
```

### 8.4 The native provider — THE agent

One agent implementation: an **in-process chat-completion loop in Rust**
([decision 001](docs/decisions/001-drop-pi.md)). The same loop serves fast conversational groups
(a small MoE chat model fronting the main chat) and coding groups (a dense coder model with
execution tools) — what differs per group is the model, the endpoint, and the **tool profile**.
Groups compose: a conversational group delegates real work to a coding group via `send_to_agent`
(agent-to-agent routing; concierge → worker with zero new machinery).

- **One client protocol: OpenAI-compatible `/chat/completions`** (`reqwest`), with tool-calling.
  That single protocol covers OpenRouter, llama.cpp, vLLM, Ollama, and most hosted gateways —
  endpoint selection is data (§8.7), not code.
- **Memory is the session DB.** No provider-side session state: the loop rebuilds context from
  `messages_in`/`messages_out` (already the full conversation) with a token-budgeted window.
  Known limitation: intermediate tool rounds are not yet persisted across turns (PLAN 4.7).
- **The agent writes its own replies**: tool calls execute against the session DB / workspace,
  results feed back to the model, capped rounds prevent loops. **Graceful degradation is
  required:** if the model emits no tool calls, plain assistant text becomes a `send_message` —
  small local models must work as chat-only agents.
- System prompt from the group's `AGENT.md`.

The `echo` provider (returns the prompt verbatim) ships in-tree so the entire pipeline — web UI →
router → queue → supervisor → delivery → web UI — tests without any API key. The `formatter`
module (batch → XML prompt, timezone-aware) is the prompt renderer for any future
prompt-consuming provider behind the same trait.

### 8.5 Agent tools & tool profiles

All tools are Rust functions in the daemon. Tool errors (bad args, unknown tool) become result
strings fed back to the model — a confused model can self-correct; a turn never fails on a tool.

| Tool | Profile | Effect |
|---|---|---|
| `send_message` | all | `messages_out` row, kind `chat` (the reply path) |
| `send_file` | all | stage file → `outbox/<msg_id>/`, row with `files` |
| `schedule_task` / task ops | all | `system` rows → host inserts/updates `messages_in` |
| `send_to_agent` | all | row with `channel_type='agent'`, `platform_id=<target group>` |
| `ask_user_question` | all | `messages_out` row with `Operation::AskQuestion` (+ text for the transcript); delivery registers `pending_questions`, the channel renders a card. The run does **not** block — the user's choice returns as a normal inbound that re-wakes the session (M7.1) |
| `bash` | coder | run a command, cwd = group workspace, timeout + output truncation |
| `read` / `write` / `edit` | coder | workspace files; `edit` = exact-string replace |
| `admin` | `cli_scope`≠disabled | run a registry command as this agent (`CallerContext::Agent`); the dispatcher re-applies `cli_scope`/`Hidden` gates (M6.3). In-chat self-service (§8.7) |

The **tool profile** is a per-group column: `chat` (messaging tools only — the safe default) or
`coder` (+ bash/files). §11's honesty still applies: profiles scope the polite interface, not a
security boundary. `grep`/`find`/`ls` are bash one-liners, not tools.

Future extensibility is **MCP-client** shaped, not extension-script shaped: per-group MCP servers
plugged into the same tool dispatch (backlog, PLAN M9+).

### 8.6 LLM endpoint & model configuration

**One `endpoints` table.** OpenRouter *is* an OpenAI-compatible endpoint
(`https://openrouter.ai/api/v1`), as are llama.cpp, vLLM, and Ollama — so claw needs exactly one
endpoint concept, configured from the web UI:

```sql
CREATE TABLE endpoints (
  name        TEXT PRIMARY KEY,     -- "openrouter", "local-llama", …
  base_url    TEXT NOT NULL,        -- OpenAI-compatible /v1 root
  api_key     TEXT,                 -- stored value, or…
  api_key_env TEXT,                 -- …name of a container env var (per-row choice)
  notes       TEXT
);
```

**Per-agent-group selection** (central DB, nullable columns):

| Column | Values | Meaning |
|---|---|---|
| `agent_provider` | `native` \| `echo` | which harness (`AgentProvider` registry — seam for future external providers) |
| `endpoint` | FK → `endpoints.name` | where inference runs |
| `model` | free text | model id at that endpoint (`gemma4-…`, `qwen3.6-…`) |
| `tool_profile` | `chat` \| `coder` | which tool surface the agent gets (§8.5) |

Resolution: group row → `CLAW_DEFAULT_MODEL`/`CLAW_DEFAULT_ENDPOINT` env → error (each missing
link is a distinct, named error). Example install: main chat group = `openrouter` + a fast
conversational MoE + `chat`; coding group = `local-llama` + a dense coder model + `coder`; the
chat group delegates heavy work to the coding group over `send_to_agent`.

**Configuration surfaces** all fall out of the command registry (§9.2): an **Endpoints** resource
(auto-generated table + form in the web admin; `claw endpoints create …` on the CLI), the groups
form gaining endpoint/model/profile fields, and in-chat self-service ("switch to gemini") via the
agent's `admin` tool — an in-process call into the same registry as `CallerContext::Agent`, gated
by `cli_scope`/approval (M6.4). The run never blocks on it; the dispatcher re-checks scope so the
tool is the seam, not the security boundary.

---

## 9. Web interface (the built-in channel)

One axum app serves everything: static UI (embedded in the binary at build time), REST API, SSE
stream, and webhook endpoints for future adapters.

**Chats** are web messaging groups: the sidebar lists the active ones, "+ new chat" creates one
(wired to the first agent group), and `groups-create` (M9.1) makes a chat wired to a *specific*
new agent. **Archiving** (M9.2, `archived_at`) drops a chat into a collapsible "archived" section
without deleting its history; the chat header toggles it. The `trigger=0` accumulate policy and
per-thread sessions live in the router/session layer but have no UI surface on the single-user web
DM (always-engage, Shared mode) — they activate when group/threaded channels land.

**Auth:** single shared secret (`CLAW_AUTH_TOKEN` env; generated and printed on first start if
unset). Login page exchanges it for an HttpOnly session cookie; every `/api` and SSE route sits
behind the middleware. The web user is `web:owner` in the entity model and is auto-granted the
`owner` role on first login — approvals and admin commands route to the UI instead of a DM.

**Data flow:** the web channel is a normal `ChannelAdapter`:

- `POST /api/chats/:id/messages` → `InboundEvent` into the router's mpsc (a web chat *is* a
  messaging group with `channel_type='web'`).
- `deliver()` → append to the **`web_messages` ledger** (central DB: per-chat rendered transcript —
  the one thing platform channels got for free that we must store) → push over the SSE hub to
  connected browsers.
- `Operation::AskQuestion` is delivered as a **question card**: a `web_messages` row of
  `kind='question'` (carrying `question_id`/`options`/`answer`, migration 004) plus a
  `pending_questions` registry row. The buttons `POST /api/questions/:id/answer`; the handler
  validates the choice against the open question, collapses the card (`answer` set → SSE
  `message_update`), and submits the choice as a normal inbound so the asking session re-wakes.
  Unanswered cards collapse to "no answer" once the sweep passes their TTL. Edits and reactions
  mutate the ledger row and push an SSE update.
- **Approvals** (M7.2) reuse the same machinery. A command marked `Access::Approval` (e.g.
  `endpoints-delete`) issued by an agent is held by the dispatcher (after the `cli_scope` check) and
  returned as `ApprovalPending`; the `admin` tool turns that into an `Operation::Approval` outbound,
  which delivery records in `pending_approvals` and the channel renders as an **approval card**
  (`kind='approval'`, Allow/Deny). `POST /api/approvals/:id/answer` runs the held command via
  `Registry::execute_approved` (no re-gate — the operator's allow *is* the authorization), then
  re-wakes the asking session with a `system` result row. The operator (Host) is never gated, so
  the same command stays immediate on the CLI. `pick_approver` is the owner, web-first: the card
  lands in the chat the agent was working in.
- Typing/progress: `Progress` events from the running session surface as a live "working…" status
  line — strictly better than typing indicators.

The `ChannelAdapter` trait survives unchanged from the first draft (`run`/`deliver`/optional
`edit`/`react`/`open_dm`, cargo-feature barrel) so platform adapters remain a one-module fork job.

### 9.1 Interface design

**Tech: server-rendered, zero frontend build step.** Askama templates rendered by axum + htmx +
one hand-written `claw.js` (~150 lines: SSE wiring, fragment swap/append by id, reconnect with
backoff, scroll pinning, composer niceties). Markdown is rendered **server-side** with
`pulldown-cmark` (sanitized); the client parses nothing and holds no state beyond "which chat is
open" — a reload always reproduces reality from the `web_messages` ledger. The fork story stays
whole: UI changes are template/CSS edits and `cargo build`, same as every other change. No Node
anywhere — not in the build, not in the image.

**Layout — one screen, three panes:**

```
┌──────────────┬──────────────────────────────────────────────┬─────────────────┐
│ claw         │  family-chat                    wired: andy  │  ▸ Agent        │
│ ● family     │  you  10:02                                  │  ■ running      │
│ ○ pr-review  │  plan a menu for saturday, 8 people          │   bash: curl …  │
│   2 queued   │  andy  10:02                                 │   00:41         │
│ ○ research   │  ┌ Any dietary restrictions? ─────────────┐  │  Queue          │
│ ──────────── │  │ [vegetarian] [gluten-free] [none]      │  │  1. pr-review   │
│ ⏰ Tasks (3) │  └────────────────────────────────────────┘  │  2. research    │
│ ✓ Approvals① │  ⟳ andy is working — Read: menu-draft.md     │  Approvals (1)  │
│ ⚙ Admin      │  [ write a message…                  ] [📎]  │  [allow][deny]  │
└──────────────┴──────────────────────────────────────────────┴─────────────────┘
```

- **Left — chats** (= messaging groups) with unread dots and **queue badges**: the sequential queue
  must be visible or silence reads as breakage. Below: Tasks, Approvals (badge), Admin.
- **Center — transcript** from the ledger. Item types: user message, agent message (markdown, file
  chips from the outbox), question card (buttons POST the answer; card collapses to the chosen
  option), and system events as thin separators ("task fired", "approval granted") — they exist as
  messages anyway; showing them makes the system legible. The user's own messages carry a status
  chip mapped from `messages_in.status`: *queued (2 ahead)* → *agent working* → gone on reply.
- **Right — agent panel**: the run supervisor made visible. Current run (session, current tool from
  `Progress` events, elapsed), the queue in order, pending approvals. Collapses to a header status
  pill on narrow screens.

**SSE protocol — one stream (`GET /api/events`), dumb client.** Fragments are rendered by the same
templates as full pages (single rendering path):

| Event | Payload | Client reaction |
|---|---|---|
| `message` | chat id + HTML fragment | append to transcript / bump unread dot |
| `message_update` | message id + HTML | swap node (edits, card collapse) |
| `run` | chat id, state, current tool, elapsed | agent panel + status chips |
| `queue` | ordered chat ids | badges + panel |
| `approval` | rendered card | approvals slot + badge |

**Visual system — dark Nord, Fira Code Nerd Font, flat.** Enforced by a single hand-written
`claw.css`, **design-token oriented**: a tokens layer of CSS custom properties on `:root`, and
component rules that consume *only* tokens — no literal colors, sizes, or font names anywhere
below the tokens block. *Flat* is taken literally (UX overhaul M10): one background, hairline
dividers (`--hair`), no raised/filled panels or bubbles, transparent flat controls with an accent
focus/hover, and status carried by colour + a 2px left-rule (questions, approvals, errors) rather
than boxes.

Two token tiers, primitives → semantic:

```css
:root {
  /* 1 ── primitives: the Nord palette, never referenced by components */
  --nord0: #2e3440;  --nord1: #3b4252;  --nord2: #434c5e;  --nord3: #4c566a;
  --nord4: #d8dee9;  --nord6: #eceff4;
  --nord8: #88c0d0;  --nord9: #81a1c1;
  --nord11: #bf616a; --nord13: #ebcb8b; --nord14: #a3be8c; --nord15: #b48ead;

  /* 2 ── semantic tokens: what components are allowed to use */
  --color-bg: var(--nord0);          --color-surface: var(--nord1);
  --color-surface-raised: var(--nord2);
  --color-border: var(--nord3);      --color-text-muted: var(--nord3);
  --color-text: var(--nord4);        --color-text-strong: var(--nord6);
  --color-accent: var(--nord8);      --color-accent-2: var(--nord9);
  --color-ok: var(--nord14);         --color-pending: var(--nord13);
  --color-error: var(--nord11);      --color-scheduled: var(--nord15);

  --font-mono: 'FiraCode Nerd Font', 'Fira Code', ui-monospace, monospace;
  --text-xs: 0.75rem; --text-sm: 0.875rem; --text-md: 1rem; --text-lg: 1.25rem;

  --space-1: 4px; --space-2: 8px; --space-3: 12px; --space-4: 16px; --space-6: 24px;
  --radius: 4px;  --border: 1px solid var(--color-border);
  --focus: 1px solid var(--color-accent);
}
```

- **Components consume semantic tokens only** (`background: var(--color-surface)`, never
  `var(--nord1)`, never `#3b4252`). Re-theming — light mode, different palette — is a fork edit
  to the `:root` block and nothing else; a grep for `#` and `px` outside the tokens block is the
  lint.
- **Palette roles:** Polar Night for surfaces, Snow Storm for text, Frost for interaction (one
  primary accent), Aurora strictly for status — `--color-ok` delivered/success, `--color-pending`
  queued/awaiting approval, `--color-error` failed/deny, `--color-scheduled` recurrence. Color
  always means state, never decoration.
- **Type: Fira Code Nerd Font everywhere** (self-hosted woff2 in `assets/`, fallback
  `'Fira Code', ui-monospace, monospace`). Monospace-everything suits the transcript-and-ids
  nature of the app; ligatures on. Hierarchy comes from size/weight/`nord6`-vs-`nord4`, not
  typeface changes. **Nerd Font glyphs are the icon set** — chat/task/gear/spinner glyphs instead
  of an SVG icon library; zero icon dependencies.
- **Flat:** no shadows, no gradients, 1px `nord3` borders, 4px radius, generous-but-dense spacing
  on a 4px grid. Left-aligned transcript with hanging sender labels — no chat bubbles; long agent
  output with code blocks reads like a document, not a messenger. Focus states are 1px `nord8`
  borders; the single animation allowed is the running-state spinner glyph.
- `prefers-color-scheme` is ignored: the theme is dark Nord, period (one fewer fork-breakable
  surface; a light theme is a CSS-variable fork edit).

Punted past trunk: token-streaming render (complete-message delivery matches the ledger model)
and mobile-first layout (responsive collapse only).

### 9.2 Admin = the command registry, rendered

No bespoke admin screens. `CommandDef` args carry display metadata (label, type, enum options,
required), and the admin section generates itself from the registry (M7.3a; UX overhaul M10):
`/admin/{resource}` shows **inline-editable rows** — each item from the no-required-args `list`
command becomes a row whose fields are the matching `<resource>-update` form, prefilled from the
row (the identity field readonly), with a `save` and — where a `<resource>-delete` exists — a
`delete` button (one `<form>`, two submit buttons carrying the command). A `<resource>-create`
renders as an "add" row; anything left over (e.g. `roles-grant`/`-revoke`) renders as a standalone
form; resources with no `update` render read-only cells (e.g. `wirings`). `ArgKind` drives inputs
(Text→input, Bool→checkbox, Enum→`<select>`; `endpoint`→a live dropdown of configured endpoints).
The field grid is a fluid `auto-fit` grid, so rows reflow on narrow screens (the sidebar collapses
to a top bar). `POST /admin/run` coerces the submitted form by each `ArgSpec` and dispatches as
`Host`. One dispatch path, two live transports — unix socket (CLI) and HTTP (web admin), plus the
in-process `admin` tool (agents, M6.4) — all under the same `cli_scope`/approval gates. Any command
a fork registers appears in the web admin with zero UI work.

**Creating agents** (`groups-create`, M9.1) is just another registry command — `Access::Approval`,
so the operator (Host) runs it directly but an agent spawning a sub-agent is held for owner approval
(§ M7.2). It inserts the `agent_groups` row (name → slugified folder), plus a default wiring: a fresh
web chat so the new agent is immediately reachable in the UI. The group folder and a starter
`AGENT.md` are scaffolded by the supervisor on the agent's first run (existing AGENT.md is never
clobbered).

The one bespoke admin view is **Tasks** (M7.3b): `/admin/tasks` scans every active session's DB
(`list_scheduled_tasks`) and tabulates group, series, prompt, schedule, next fire (recurring →
cron evaluator, one-shot → `process_after`), and status, with pause/resume/cancel posting to
`/admin/tasks/action` (which reopens the session DB and flips/cancels the series). The page you
actually check daily.

---

## 10. Routing, delivery, sweep, central DB, admin CLI

These port from the first draft with containers subtracted:

- **Router** (`router.rs`): messaging-group lookup/auto-create, session resolution, and the
  **engage decision** (`engage.rs`, M8.2): non-chat kinds and direct messages always run; group
  chats consult the wiring's `engage_mode` — `Pattern` (case-insensitive substring for now; regex
  is a future dep), `Mention`, `MentionSticky` (sticky session-state deferred until group channels
  land). A non-engaging message is still written but with `trigger=0` — it **accumulates** and rides
  along on the next engaging run instead of waking the agent. "Wake" means `RunQueue::enqueue`, done
  only for engaging messages. Dropped (no-wiring) messages go to the `dropped_messages` ledger. (The
  pi-era host-side command gate is N/A — the native loop has no slash commands; see M6.3.)
- **Delivery** (`delivery.rs`): poll `messages_out` (1s while a run is live for that session, 60s
  sweep over all active sessions), filter against `delivered`, dispatch: `system` → action registry;
  `channel_type='agent'` (delegation, `platform_id`=target group) → write into a per-source worker
  session + set its return routing + enqueue; `channel_type='agent-return'` (worker reply,
  `platform_id`=originating session id) → write into that exact session + enqueue, so the concierge
  relays the result to the user (M15); otherwise → destination permission check + `adapter.deliver()`
  + ledger + outbox cleanup. 3 attempts → failed.
- **Sweep** (`sweep.rs`): due `process_after` rows → enqueue; due `deliver_after` → deliver;
  recurrence advance computed grid-aligned via the in-house jiff cron evaluator, same `series_id`;
  run watchdog (§8.2).
- **Central DB**: NanoClaw's schema minus container tables: `agent_groups` (+ per-group
  provider/endpoint/model config, §8.7), `messaging_groups`, `messaging_group_agents`, `users`,
  `user_roles`, `agent_group_members`, `sessions`, `pending_questions`, `dropped_messages`, plus
  **`endpoints`** (§8.7), **`web_messages`** (§9) and `schema_version`. One file per entity, numbered migrations, WAL, single pooled connection.
- **Admin CLI**: `claw groups list`, `claw wirings create …` — clap subcommands in the same binary,
  speaking newline-framed JSON over `/data/claw.sock` (mode 0600) to the running daemon. Command
  registry with `Access::{Open, Approval, Hidden}`. Agent-side CLI access works the NanoClaw way —
  `system` messages_out rows with scope enforcement (`disabled|group|global` per agent group) — and
  needs no socket.

---

## 11. Security model — stated plainly

NanoClaw's premise is OS-level isolation per agent. rust-claw trades that away deliberately:

- A coder-profile agent runs `bash` as the same user, in the same container, on the same
  filesystem as the app. It can read `central.db`, other sessions' folders, and the app binary.
  Workspace `cwd`, tool profiles, and prompts scope it by *convention*, not enforcement.
- What the Docker boundary still buys: the agent cannot touch the host machine, its credentials
  beyond the env vars you pass in, or anything outside the container + `/data`.
- Consequences embraced: approval gates (`create_agent`, destinations, admin commands) are
  intent-checks on the cooperative path, not security boundaries; `cli_scope` and tool profiles
  limit the polite interface, not a determined agent.
- Mitigations kept cheap and real: containment-checked attachment staging, destination allowlists
  validated host-side, the approval flow for self-modifying actions, and single-user auth on the
  only network surface.

If per-agent isolation is ever wanted back, the seams are preserved: the provider spawn is one
function, and the single-session DB schema works across a mount boundary too (it would need the
upstream pragma discipline back — see the dropped-invariants table in §5 for exactly what to restore).

---

## 12. Errors, logging, config

- `thiserror` enums per domain (`ChannelError`, `ProviderError`, `ContentError`, `FrameError`);
  `anyhow` only in `main` and task bodies. Loop iterations are fallible-and-logged, never
  loop-fatal: one poisoned session can't stop delivery for the rest.
- `tracing` + `tracing-subscriber`: single-line structured events, stderr + `/data/logs/claw.log`,
  `WARN+` duplicated to `claw.error.log`. Tool subprocess stderr (bash) is captured into tool
  results and logs — agent failures are never lost.
- Config: shared values in `config.rs` (`DATA_DIR=/data`, `PORT`, `CLAW_AUTH_TOKEN`, `TIMEZONE`,
  watchdog/backoff constants). Module-specific values read where used. Everything else is code.

---

## 13. Dependencies

(⚠️ run the supply-chain check on exact versions at `cargo add` time.)

| Crate | Why |
|---|---|
| `tokio` (rt-multi-thread, process, net, sync, time, signal) | runtime, bash tool children, unix socket |
| `axum` + `tower-http` | web UI, API, SSE, static assets |
| `rusqlite` (bundled) | central + session DBs; sync behind `spawn_blocking` |
| `serde`, `serde_json` | content blobs, RPC frames, API |
| `thiserror` / `anyhow` | errors (libs / binary) |
| `tracing`, `tracing-subscriber` | structured logs |
| `clap` (derive) | `serve` + admin subcommands |
| `jiff` | timezone-aware timestamps (IANA tz for the formatter) |
| (in-house) | cron recurrence — a small jiff-based evaluator, no chrono dependency |
| `ulid` | sortable ids |
| `tokio-util` | `CancellationToken` |
| `regex` | engage patterns, command gate |
| `async-trait` | `dyn ChannelAdapter` |
| `reqwest` (rustls, json, stream) | native provider's OpenAI-compatible client |
| `askama` | server-side templates (pages + SSE fragments, one rendering path) |
| `pulldown-cmark` (+ sanitizer) | server-side markdown for agent messages |
| `rust-embed` (or `include_dir`) | UI assets (css, js, Fira Code woff2) in the binary |

No new crate for MCP (M12): the stdio client (`src/mcp/`) is hand-rolled on `tokio` + `serde_json`,
so `rmcp` stays out of the tree. The bundled web-search MCP server is a **separate Rust binary**
(`mcp-web-search-stdio`, built from a pinned upstream commit in the Docker image) that claw spawns
over stdio — still 100 % Rust and no second *language* runtime (decision 001). The one real cost is
that the runtime image now ships a headless **Chromium** (a few hundred MB) for that server.
No Node, no TypeScript, no JS package manager anywhere in the project.

---

## 14. Testing

- **Pure functions first**: engage evaluation, command gating, backoff/watchdog decisions,
  recurrence advance, seq assignment, formatter output, attachment-name safety. Table-driven `#[test]`.
- **Session-DB contract tests**: writer/reader pairs against tempdir DB files — seq parity,
  due-filtering, the delivered ledger. The DDL lives once in `src/session/schema.sql`.
- **End-to-end without keys**: spin the full daemon against a temp `/data` and drive it through
  the real HTTP API — echo provider for the pipeline, an in-process mock OpenAI server for the
  native provider (fallback path, tool-call path, and the coding tools). `#[ignore]` variants run
  against a real model endpoint.

---

## 15. Deviations from NanoClaw

| # | Deviation | Rationale |
|---|---|---|
| 1 | App dockerized, agents as child processes — no per-agent containers | single-user simplicity; the security trade is documented in §11; isolation seams preserved |
| 2 | One `session.db` (WAL) instead of inbound/outbound split with DELETE-journal discipline | the split existed for cross-mount SQLite; same-kernel local FS removes the problem (§5) |
| 3 | Global sequential run queue, drain-and-exit; no warm/idle containers | predictable load on small hardware; deletes warm-pool, idle-timeout, and concurrent-stuck logic |
| 4 | Built-in web UI is the primary channel; Chat SDK bridge dropped | richest channel becomes the one we own; requires the `web_messages` ledger (platforms stored history for us before) |
| 5 | Home-made in-process agent loop instead of any external harness (Claude Agent SDK, pi) | by M3 the loop/client/memory existed; the remaining delta was bash + file tools — see [decision 001](docs/decisions/001-drop-pi.md) |
| 6 | Agent tools are Rust functions in the daemon; future extensibility is MCP-client shaped | tools remain "writes to the session DB"; no extension-script language, no second runtime |
| 7 | Single binary (`claw serve` + admin subcommands), protocol as a module | one process model no longer justifies a workspace |
| 8 | No OneCLI / credential proxy | inference keys are env vars or endpoint rows; acceptable for the single-user threat model |
| 9 | `heartbeat`/`processing_ack`/`on_wake` machinery deleted | the supervisor owns the run in-process; the races those solved cannot occur |
| 10 | Tool subprocess output captured into tool results + logs | fixes upstream's lost-container-logs gap for free |

Everything not listed — entity model, engage/gating semantics, message kinds and XML formatting,
scheduling-as-messages, approval routing, command registry and `cli_scope`, outbox/inbox
conventions, seq parity — is a faithful port.

---

## 16. Milestones

Each ends green (`cargo test`) and runnable.

1. **M1 — Skeleton + contract.** Crate, `protocol/` types, central DB + migrations, session folder +
   `session.db` access layer. Contract tests pass.
2. **M2 — Vertical slice.** Router (minimal), RunQueue + supervisor with **echo provider**, delivery,
   minimal web UI (login, one chat, SSE). A browser message round-trips end to end. No Node needed.
3. **M3 — Native provider + endpoints.** `endpoints` table + resolution, the in-process
   chat-completion loop with session-DB context window and native messaging tools, graceful
   no-tool-call degradation. First real conversation (OpenRouter or local endpoint).
4. **M4 — Coding agent.** Decision 001 cut (pi + TypeScript removed); `bash` tool (workspace cwd,
   timeout, output truncation) + per-group tool profiles; `read`/`write`/`edit` file tools;
   coding-group end-to-end. Chat↔coder delegation via `send_to_agent`.
5. **M5 — Scheduling + resilience.** Sweep, `schedule_task` + recurrence, watchdog + backoff,
   crash recovery, circuit breaker.
6. **M6 — Admin surface.** Command registry, socket server, `claw` subcommands, roles/membership,
   command gate, agent `cli_scope`.
7. **M7 — Interactivity + approvals.** `ask_user_question` cards in the UI, pending_questions,
   approvals module, web admin page.
8. **M8 — Packaging.** Dockerfile (multi-stage, pure Rust image), compose file, `/data` bootstrap,
   first-run token flow, upgrade path.
9. **M9 — Extended messaging + extensibility.** `create_agent`, multi-group web chats, per-thread
   sessions in the UI. Backlog: MCP client as the tool-extensibility seam; claw-as-MCP-server as
   an additional control surface.
