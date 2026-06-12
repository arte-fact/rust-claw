# Implementation Plan

Source of truth for execution. One subtask = one focused session: it starts from a green tree and
ends with tests passing, `cargo clippy --all-targets -- -D warnings` clean, and its box checked.
Design rationale lives in [ARCHITECTURE.md](ARCHITECTURE.md) (referenced per subtask).

Definition of done for every subtask, in addition to its listed deliverable:
tests written first (TDD), zero clippy warnings, no `#[allow]`/`#[expect]`, files split before they
grow large, PLAN.md checkbox ticked. **UI-touching subtasks (2.6, 7.1, 7.3, 9.2, …) additionally
require visual verification via the snapshot/debug MCP tools and a refreshed `screenshots/`
folder — see CLAUDE.md §"UI verification".**

## M1 — Skeleton + contract

- [x] **1.1 Project scaffolding.** Crate layout per §4 (empty modules), `clap` skeleton
      (`claw serve` stub, `--version`), `config.rs` (env parsing, defaults, `/data` paths),
      `tracing` setup (stderr + file layers). Deliverable: `claw serve` boots, logs, exits clean
      on SIGTERM. `justfile`/CI recipe: fmt-check, clippy `-D warnings`, test.
- [x] **1.2 Protocol: ids + enums.** `string_id!` macro (Display/FromStr/serde/ToSql/FromSql),
      all newtypes; `MessageKind`, `MessageStatus`, `SessionMode`, `EngageMode`, `CliScope` with
      kebab-case SQL/serde round-trip tests. (§6)
- [x] **1.3 Protocol: message content.** `InboundContent`/`OutboundContent`/`Operation`/`Routing`
      with parse-by-kind + serialize round-trip tests, including the `Raw`/`extra` escape hatches. (§6)
- [x] **1.4 Central DB core.** Connection (WAL, pooled), migration runner (`schema_version`,
      numbered, transactional) + `001-initial`; `agent_groups` + `messaging_groups` +
      `messaging_group_agents` CRUD with tests on temp DBs. (§10)
- [x] **1.5 Central DB entities.** `users`/`user_roles`/`agent_group_members`/`user_dms`,
      `sessions`, `endpoints`, `dropped_messages`, `pending_questions` CRUD + tests. (§8.7, §10)
- [x] **1.6 Session store.** Session folder init (dirs + `session.db` schema §7), access layer:
      `write_message` (even seq), status transitions, due-pending/due-outbound queries, `delivered`
      ledger, `session_routing`/`destinations`. Contract tests incl. seq parity. (§5, §7)

## M2 — Vertical slice (echo agent, browser round-trip)

- [x] **2.1 RunQueue + supervisor.** FIFO + dedup + `Notify`; supervisor drain loop with the
      `AgentProvider` trait + `echo` provider; status transitions `pending→processing→completed`,
      reply rows written. Tests: ordering, dedup, mid-run arrivals drained, failure path. (§8.1–8.3)
- [x] **2.2 Router (minimal).** `InboundEvent` mpsc → messaging-group lookup/auto-create →
      session resolution (`Shared` mode) → write + enqueue. Engage modes deferred (always engage).
      Tests. (§10)
- [x] **2.3 Delivery + ChannelAdapter trait.** Poll due `messages_out`, `delivered` filter,
      dispatch via trait; retry counter → failed at 3; outbox file pickup + cleanup. Test adapter
      (in-memory). (§9, §10)
- [x] **2.4 Web foundation.** axum app, token login → HttpOnly cookie middleware, embedded assets
      (`rust-embed`), health route. Auth tests (401/redirect/cookie). (§9)
- [x] **2.5 Web channel.** `web_messages` ledger, chats + messages REST, POST message →
      `InboundEvent`, SSE hub (`message`/`message_update`/`run`/`queue` events), the web
      `ChannelAdapter` (`deliver` → ledger + SSE). Integration tests over HTTP. (§9, §9.1)
- [x] **2.6 Minimal UI.** Askama templates (login, shell, chat list, transcript, composer),
      `claw.css` tokens block (§9.1) + components, `claw.js` (SSE wiring, swap/append, scroll).
      E2E test: login → post → echo reply arrives over SSE. Visual verification: snapshot login +
      chat shell via the MCP screenshot/interact tools, iterate until §9.1-conformant.
      Milestone exit: usable in a browser.

## M3 — Native provider + endpoints

- [x] **3.1 Endpoints.** `endpoints` CRUD commands (list/create/update/delete), api_key vs
      api_key_env resolution, group→default→provider resolution chain
      (`CLAW_DEFAULT_MODEL`/`ENDPOINT`). Tests. (§8.7)
- [x] **3.2 OpenAI-compatible client.** `reqwest` chat-completions client: request/response types,
      auth header, error mapping (retryable vs terminal), timeout. Tests against an in-process
      axum mock server. (§8.5)
- [x] **3.3 Native provider: conversation.** Context-window builder from `session.db` (token
      budget, newest-first truncation), `AGENT.md` system prompt, plain-reply loop → `send_message`
      degradation path. Mock-endpoint tests incl. window truncation goldens. (§8.5)
- [x] **3.4 Native provider: tools.** Tool schemas + dispatch for `send_message`,
      `ask_user_question` (defer blocking resolution to M7 — return placeholder), `schedule_task`,
      `send_to_agent`; multi-turn tool loop; no-tool-support fallback. Tests. Milestone exit:
      real conversation against OpenRouter/local endpoint. (§8.5)

## M4 — Coding agent (pivot: [decision 001](docs/decisions/001-drop-pi.md))

- [x] **4.1–4.3 pi RPC client, formatter, TS extension.** Built and tested, then superseded by
      decision 001 (pi dropped). The formatter survives as the generic prompt renderer; the rest
      lives in git history (`01f65dc`).
- [x] **4.4 Remove pi & TypeScript.** Delete `src/providers/pi`, `pi-extension/`, the Node CI
      job, the `Pi` enum variant, the session `pi/` subdir; sync ARCHITECTURE/PLAN/CLAUDE.md;
      write the decision record. Repo becomes 100 % Rust.
- [x] **4.5 Bash tool + tool profiles.** `bash` tool for the native loop: `tokio::process`, cwd =
      group workspace, timeout, head+tail output truncation, exit code in the result. Per-group
      `tool_profile` column (`chat` | `coder`) gating the tool surface; migration. (§8.5)
- [x] **4.6 File tools.** `read` (line numbers/limits), `write` (create/overwrite), `edit`
      (exact-string replace, unique match required) — workspace-rooted path resolution. Table
      tests. (§8.5)
- [x] **4.7 Coding-group end-to-end.** Integration test: coder-profile group, mock LLM scripted
      to call `bash` + `edit` then reply; `#[ignore]` real-model test. Decide + document the
      cross-turn tool-memory approach (transcript currently persists chat only). Milestone exit:
      chat↔coder delegation via `send_to_agent`. (§8.4, §8.5)

## M5 — Scheduling + resilience

- [x] **5.1 Sweep.** 60s task: due `process_after` → enqueue, due `deliver_after` → deliver,
      recurrence advance (in-house jiff cron, grid-aligned, `series_id`). Time-injected tests. (§10)
- [x] **5.2 Task lifecycle.** `schedule_task` system action → `messages_in` insert;
      list/cancel/pause/resume/update actions; task formatting (`<task>`, pre-script,
      `wake_agent=false`). End-to-end schedule-fire-reply test. (§8.6)
- [x] **5.3 Watchdog + recovery.** No-event watchdog (pure decision fn + kill), retry backoff
      (`5s·2^tries`, 5 → failed), startup `processing→pending` reset, circuit breaker file.
      Tests for every decision branch. (§8.2)

## M6 — Admin surface

- [x] **6.1 Socket server + dispatch.** Frame codec, `UnixListener` (0600, stale unlink),
      dispatch with `CallerContext`, error codes. Temp-socket tests. (§10)
- [x] **6.2 Command registry + CLI client.** `CommandDef` with arg display metadata (§9.2),
      generic CRUD generation per resource, `claw <resource> <verb>` subcommands + table output.
      Tests: registry → CLI → socket → dispatch round-trip.
- [x] **6.3 Command gates.** `cli_scope` enforcement (disabled/group/global, whitelisted
      resources, own-group auto-fill/cross-check, no self-`cli_scope` change), `Access::Hidden`
      operator-only gating, and `roles` resource (list/grant/revoke, owner-global invariant).
      Enforced at the `Dispatcher` so every caller path is gated; tested in isolation and through
      the real registry with an `Agent` caller. The router "command gate" is N/A post-pi (the
      native loop has no slash commands the router interprets); revisit if operator slash-commands
      land. (§10)
- [x] **6.4 Agent-facing CLI transport.** An `admin` tool reaches the command registry as
      `CallerContext::Agent`: the supervisor threads an `AgentAdmin` (a `Dispatcher` handle + caller
      identity) into `QueryInput` → the native `ToolContext`, but only when the group's `cli_scope`
      ≠ `disabled` (so the tool is offered exactly when it can be used). The dispatcher re-applies
      the 6.3 gates, so in-chat self-service ("switch to gemini") works under `cli_scope`. Tests:
      tool presence by capability, refusal without it, dispatch-through-the-gate (allow + group-scope
      refusal), and a full scripted-LLM e2e mutating the agent's own group via the `admin` tool.
      (§8.5, §8.7, §10)

## M7 — Interactivity + approvals

- [x] **7.1 ask_user_question end-to-end.** `ask_user_question` writes an `Operation::AskQuestion`
      outbound (+ transcript text); delivery registers `pending_questions`; the web channel renders
      a `kind='question'` card row (migration 004) over the existing `message` SSE. `POST
      /api/questions/:id/answer` validates the choice, collapses the card (`message_update` SSE),
      and re-wakes the session with the answer as a normal inbound — the run never blocks (fits the
      global sequential queue). Unanswered cards expire via the sweep TTL. Visual verification:
      `screenshots/question-card.png` (open + collapsed states). (§8.5, §9.1)
- [x] **7.2 Approvals.** `Access::Approval` commands issued by an agent are held by the dispatcher
      (after the `cli_scope` check) and returned as `ApprovalPending`; the `admin` tool surfaces them
      as an `Operation::Approval` outbound → `pending_approvals` (migration 005) → an Allow/Deny
      **approval card** (reuses the M7.1 card + SSE, `kind='approval'`). `POST /api/approvals/:id/answer`
      runs the held command via `Registry::execute_approved` (no re-gate — the operator's allow is the
      authorization) and re-wakes the session with a `system` result row; the operator (Host) stays
      ungated. `pick_approver` = owner, web-first (card lands in the originating chat). `endpoints-delete`
      is the first `Approval` command. Tests: gate holds vs. executes, `execute_approved`, and a full
      scripted-LLM e2e (agent delete held → owner Allow → endpoint removed). Screenshot:
      `screenshots/approval-card.png`. (§9.1, §10)
- [x] **7.3a Web admin — registry-driven resources.** `/admin/{resource}` pages generated from the
      command registry: a list table (from the no-required-args `<resource>-list`) plus a form per
      mutating command (reads — `-list`/`-get` — excluded), with `ArgKind` driving inputs
      (Text→input, Bool→checkbox, Enum→select; the `endpoint` field is a live dropdown of configured
      endpoints). `POST /admin/run` coerces the form per `ArgSpec` and dispatches as `Host` (operator —
      so `Hidden` commands like `roles-grant` are available here); failures redirect back with a flash.
      Linked from the chat sidebar. Tests: page renders forms/tables, create-runs-as-Host, Enum select,
      Hidden-shown, unknown-resource redirect. Screenshots: `admin-endpoints.png`, `admin-groups.png`
      (full dropdowns), `admin-roles.png`. (§9.2)
- [x] **7.3b Web admin — Tasks page.** `/admin/tasks` scans every active session's DB
      (`list_scheduled_tasks`) and tabulates group/series/prompt/schedule/next-fire/status; next fire
      is computed by the cron evaluator for recurring tasks and `process_after` for one-shots.
      pause/resume/cancel post to `/admin/tasks/action`, which reopens the session DB and flips/cancels
      the series. Shares the admin sidebar (a `tasks` nav entry). `WebState` gained the `SessionStore`
      + timezone. Tests: list + pause + cancel round-trip. Screenshot: `admin-tasks.png`. (§9.2, §10)

## M8 — Packaging

- [x] **8.1 Image + compose.** Multi-stage `Dockerfile` (rust:1.96 builder with `build-essential`
      for rusqlite's bundled SQLite → debian-slim + binary + CA certs + bash/git/curl; no Node), with
      a dep-cache layer (`touch` busts cargo's mtime fingerprint after COPY). `compose.yaml` with the
      `/data` volume + env. First-run token: `resolve_auth_token` persists a generated token to
      `/data/auth_token` (0600) and logs it, reused across restarts; an explicit `CLAW_AUTH_TOKEN`
      wins and isn't written. `/data` bootstrap via the logs/dir creation on boot. `scripts/smoke.sh`
      builds + boots the image and asserts healthz + the auth gate — **passes**. (§2)
- [x] **8.2 Ops polish.** **Engage modes** (`engage.rs` pure decision + router wiring): non-chat &
      DMs always run; group chats apply the wiring's `engage_mode` (`Pattern` substring / `Mention` /
      `MentionSticky`), non-engaging messages accumulate (`trigger=0`) and ride the next run; sticky
      session-state deferred with group channels, `Pattern` is substring until a regex dep is
      justified. **Migrations-on-boot**: the runner already applies each missing migration
      transactionally — added an explicit partial-upgrade test. **README** quickstart + env table +
      **backup notes** (stop-snapshot-start the volume). Tests: 6 engage-decision cases, group-chat
      accumulation in the router, partial-DB upgrade. (§10)

## M9 — Extended messaging + extensibility

- [x] **9.1 create_agent.** `groups-create` registry command (`Access::Approval`): inserts the
      `agent_groups` row (name → slugified folder, optional provider/endpoint/model/profile/scope) +
      a default wiring (a fresh web chat so the agent is reachable). Operator (Host) runs it directly;
      an agent spawning a sub-agent is held for owner approval (reuses M7.2) and reaches it via the
      M6.4 `admin` tool. The folder + a starter `AGENT.md` are scaffolded by the supervisor on first
      run (never clobbering an edited one). Tests: Host creates row+chat+wiring, agent path is held,
      scaffold writes/keeps AGENT.md, and an HTTP test (admin creates an agent → its chat appears). (§8.6)
- [x] **9.2 Multi-chat UI.** Multiple web chats: **create** (sidebar form + M9.1 per-agent chats)
      and **archive/unarchive** (migration 006 `archived_at`; `set_archived`; `POST
      /api/chats/:id/archive`). Archived chats drop from the active list (and `/api/chats`) into a
      collapsible "archived (N)" sidebar section; the chat header carries an archive/unarchive button.
      Per-thread sessions and the `trigger=0` accumulate **policy are surfaced in the router**
      (M8.2) but have no UI surface on the web DM (Shared mode, always-engage) — deferred with group
      channels. Tests: `set_archived` toggle, archive HTTP round-trip (drops from list, shows under
      "archived", restores). Screenshot: refreshed `chat.png` (multi-chat list + archived section +
      archive button).

## M10 — UX overhaul

- [x] **10.1 Flat redesign + editable admin.** Global flat pass on `claw.css`: one background,
      hairline dividers (`--hair`), transparent flat controls (accent focus/hover), no filled
      panels/bubbles, status as colour + a 2px left-rule (cards/errors); a `@media` breakpoint
      collapses the sidebar to a top bar. The admin becomes **responsive inline-editable lists**:
      each `<resource>-list` item is a prefilled `<resource>-update` form-row (identity readonly) with
      save + (where it exists) delete as two submit buttons on one form; `<resource>-create` is an
      "add" row; leftover commands (roles grant/revoke) are standalone forms; no-`update` resources
      (wirings) render read-only cells. Shared `admin-nav.html` / `admin-field.html` partials. All
      eight `screenshots/` refreshed; existing admin tests still pass (same command values/selects).

## M11 — Browse an agent's workspace from the web UI

Per-chat filesystem browser on **coder** agents (the chat header gets a **files** link only when
its wired agent is `ToolProfile::Coder`); full read/write. The owner is never gated, but the
browser is jailed to one agent's folder server-side — the client is never trusted.

- [x] **11.1 Jailed workspace service** (`src/workspace/`). New web-agnostic module: `path.rs`
      `jail(root, rel)` — lexical `.`/`..` collapse that refuses to climb above root, then a real-path
      check that canonicalizes the deepest existing ancestor so an in-workspace symlink pointing out
      is caught (target need not exist, for creates). Deliberately **not** `providers::native::files`,
      which lets absolute paths + `..` through by design. `mod.rs` `Workspace` (canonical root) +
      `WorkspaceError` (thiserror). `ops.rs`: `list` (kinds via `symlink_metadata`, dirs-first then
      name; `Entry` is `Serialize`), `read_text` (binary + `FILE_VIEW_MAX_BYTES` guards), `read_bytes`
      (download), `write_text`/`mkdir`/`delete`/`rename` (root-target guarded). Tests: table-driven
      jail (`..`, absolute, `a/../../b`, symlink escape, valid nested) + tempdir ops. (§4, §11)
- [x] **11.2 Read-only per-chat browser UI.** `WebState.groups_dir`; `files::coder_folder` helper
      (chat platform_id → messaging group → top wiring → coder `AgentGroup`, `None` otherwise); the
      chat header shows a **files** link only when `current.is_coder`; extracted a `chat-nav.html`
      sidebar partial shared by `shell.html` + `files.html`. `GET /chats/{id}/files` page +
      `GET /api/chats/{id}/files/{list,read}` (jailed via M11.1, non-coder/unknown ⇒ 404, binary/
      oversize ⇒ 415, traversal ⇒ 400). `claw.js` browser (breadcrumb + dir nav + `<pre>` viewer,
      names escaped before innerHTML) + `.fs-*` tokens-only CSS (incl. a mobile stack). Unit test
      for the coder gate; verified end-to-end over HTTP + Playwright. Screenshot: `files.png`. (§11)
- [x] **11.3 Mutations.** Routes (all `coder_folder`-gated + jailed): `POST …/files/{write,mkdir,
      rename,delete}` (JSON), `POST …/files/upload?path=` (raw `Bytes` body — chosen over the `axum`
      `multipart` feature, so **no Cargo change**; 32 MiB `DefaultBodyLimit`), and
      `GET /chats/{id}/files/raw?path=` (download, `Content-Disposition` attachment). `ops` gained
      `write_bytes` (binary uploads; `write_text` now delegates to it). UI: a directory toolbar
      (+ file / + folder / upload, one shared inline name input), hover ✎ rename / ✕ delete with an
      inline two-step confirm (no native dialogs), and a `<textarea>` editor with save + download.
      Verified every endpoint over HTTP (write/read, mkdir + 409, rename, upload, download, delete,
      escape→400, root→400) + Playwright; refreshed `screenshots/files.png`. (§11)

## M12 — Web tools for agents via MCP

Plug [`arte-fact/mcp-web-search-hacks`](https://github.com/arte-fact/mcp-web-search-hacks) — a Rust
stdio MCP server (`mcp-web-search-stdio`) exposing `fetch`/`search`/`screenshot`/`interact`, all
backed by a headless Chromium — into the native loop as the first `src/mcp/` consumer. **Decisions
(with the user):** hand-rolled stdio client (no `rmcp` crate); tools exposed to **all** agents;
**no config surface** — the web-search server is hardcoded and enabled by default. Still 100 % Rust
(no second *language* runtime, decision 001); the cost is shipping Chromium (~300 MB) in the image.

- [x] **12.1 Hand-rolled stdio MCP client** (`src/mcp/`). No new dependency — `tokio` (`process` +
      `io-util`) + `serde_json`. `conn.rs`: a generic `Conn<W, R>` speaking newline-delimited
      JSON-RPC 2.0 (the MCP stdio transport) — `initialize` + `notifications/initialized`,
      `tools/list`, `tools/call` (replies matched by id, interleaved notifications skipped), plus
      `flatten_content` (text blocks joined, non-text noted). `mod.rs`: `McpClient::spawn` runs the
      child, handshakes, caches the tool list; serializes calls behind a `tokio::sync::Mutex` (Chrome
      is single-session); `qualify`/`dequalify` give `<server>__<tool>` namespacing. `McpError`
      (thiserror); a failed spawn is returned so the caller runs without it. Tests: full
      handshake/list/call round-trip over `tokio::io::duplex` mock + flatten + namespacing.
- [x] **12.2 Bridge MCP tools into the native loop.** `FunctionDefinition`
      (`providers/native/client.rs`) name/description → `Cow<'static, str>` (call sites unchanged via
      `impl Into`). `ToolContext` + `QueryInput` gain `mcp: Option<Arc<McpClient>>` (manual `Debug`
      on `McpClient`); `definitions(profile, admin_enabled, mcp)` appends namespaced MCP defs **for
      every agent** (`mcp_definitions(server, tools)` pure + tested, null schema → `{type:object}`);
      `dispatch()` routes `<server>__<tool>` to `McpClient::call` (errors → result strings, never
      panics — mirrors admin). `app.rs::connect_web_mcp` spawns the client best-effort at boot and
      `Supervisor::with_mcp` threads it → `QueryInput` → `ToolContext` (a failed spawn ⇒ `None`, no
      gating). ARCHITECTURE §4 `mcp/` entry added.
- [x] **12.3 Dockerfile + docs.** New `mcp-builder` stage clones the pinned upstream commit
      (`MCP_WEB_SEARCH_REF`) and `cargo build --release -p mcp-web-search-stdio`; runtime stage
      apt-installs `chromium`, copies the binary to `/usr/local/bin`, and sets
      `CHROME_PATH=/usr/bin/chromium` (the server already launches with sandbox off). README gains a
      web-access feature bullet + a security note (agents reach the internet; Chromium adds ~MB);
      ARCHITECTURE §13 notes the hand-rolled-no-`rmcp` choice + the Chromium deviation. Verified
      end-to-end: stdio binary builds; piping JSON-RPC to the real binary returns the 4 tools and a
      live `search` result (`content` text blocks); booting claw with the binary on PATH logs
      `web-search mcp server connected tools=4`.

## M13 — Log viewer in the admin section

A live `/admin/logs` view so an operator can watch the daemon from the web UI (no shell / `docker
logs`). Source: an in-memory ring buffer (last ~1000 records) filled by a custom `tracing` layer;
`logs/claw.log` keeps the durable history. New lines stream over SSE. Controls: level filter
(colour-coded), text search, target/module filter, pause + clear.

- [x] **13.1 In-memory log buffer + tracing layer** (`src/logs.rs`). `LogRecord { seq, ts, level,
      target, message }` (`Serialize`, `time()` → `HH:MM:SS`); `LogBuffer` = `Mutex<VecDeque>` + own
      `broadcast::Sender` + `AtomicU64` seq + cap (`push` → trim front → broadcast; `snapshot`;
      `subscribe`). `LogLayer` impls `tracing_subscriber::Layer` (`on_event` → metadata + a `Visit`
      that grabs `message` and appends other fields as `key=value`). `logging::init` returns
      `(WorkerGuard, Arc<LogBuffer>)` and `.with(LogLayer)`; `main.rs::serve` threads it into
      `app::build_with_logs`; `build(config)` stays as a default-buffer wrapper so test callers are
      untouched; `WebState.logs`. Tests: capacity eviction, snapshot order + seq, subscribe delivery.
- [x] **13.2 Admin logs page + SSE + UI.** `src/web/logs.rs`: `page` (snapshot + nav, `logs_active`)
      and `stream` (`BroadcastStream(subscribe)` → `Sse`, one `log` event per record, mirrors
      `sse::events`). Routes `GET /admin/logs` + `/admin/logs/stream`; **logs** nav entry in
      `admin-nav.html` (+ `logs_active` on `AdminPage`/`TasksPage`). `templates/logs.html` (toolbar:
      min-level / module / search + pause/clear; server-rendered snapshot lines, auto-escaped).
      `claw.js` logs controller (EventSource → append + level∧target∧search filter + 2000-line cap +
      pinned autoscroll/pause/clear). `claw.css` `.log-*` tokens-only (levels →
      `--color-error`/`--color-pending`/`--color-accent-2`/`--color-text-faint`). Verified
      end-to-end (snapshot render, structured fields captured, **live ERROR streamed in over SSE**,
      level filter 6→2); `screenshots/logs.png`; ARCHITECTURE §4 entry. (§4)

## M14 — Chat activity indicator + error reporting

Presentation-only chat feedback, **additive to the all-as-message model and never touching the
session ledger** (`messages_in`/`messages_out`, seq parity, `trigger`): a phase-aware activity
indicator while a run is live, and an error card when a run fails. The activity signal is ephemeral
SSE; the error card is a `web_messages` row (the web presentation ledger the agent never reads).

- [x] **14.1 Phase-aware activity indicator** (ephemeral SSE — wired the dead `onRun`/`#chat-status`
      stub). Native loop (`providers/native/mod.rs`) threads `event_tx` into `run_turn` and emits
      `ProviderEvent::Progress` before each model call ("thinking…") and each tool dispatch (a
      `tool_action(name)` phrase, e.g. bash→"running a command", `web__search`→"searching the web").
      `RunNotifier` trait (`src/runs/supervisor.rs`): `run_state(mg_id, busy, detail)` +
      `run_failed`. `WebNotifier` (`src/web/notify.rs`) resolves mg → web `platform_id` and
      `hub.publish("run", {chat, state, detail})` (read-only DB). Supervisor gets
      `notifier: Option<Arc<dyn RunNotifier>>` + `with_notifier`; `run_session` (now a wrapper over
      `run_drain`) publishes busy before drain + idle on every exit, and `consume_run` forwards
      `Progress` via an `on_progress` callback. Fire-and-forget `spawn_blocking`. `app.rs` wires it.
      `claw.js` shows the phase in `#chat-status` + an in-transcript dots row; `.typing` CSS.
      Verified: live `run` events `working`("thinking…")→`idle` over `/events`.
- [x] **14.2 Error reporting card** (presentation-only, persisted in `web_messages`).
      `MessageRowKind::Error` + `append_error(conn, mg_id, detail)` (kind='error', no migration;
      **dedups consecutive identical errors** → one card even when the sweep retries a misconfigured
      agent). `render::message_html` gains `Error => error_html` + `templates/error.html`;
      `.msg--error` CSS (left rule `--color-error`). `WebNotifier::run_failed` appends + publishes
      `message` (live + survives reload). Reported both in the turn-failure branch **and** in the
      wrapper for pre-loop failures (the common "no endpoint" case). Tests: `append_error` round-trip
      + dedup, `error_html` escaping, `WebNotifier` (card row + `run`/`message` events), a recording-
      `RunNotifier` supervisor test (busy→failed→idle). Verified end-to-end (live error card +
      persisted, session `messages_in/out` untouched). `screenshots/chat.png`.

## M15 — Agent-to-agent delegation round-trip

A bug fix earlier made `send_to_agent` deliver into the target group's session, but the worker's
reply dead-ended (routed to a `messaging_group_id=NULL` session, not the user's chat). This completes
the **concierge-relay** round-trip so a worker's result reaches the user.

- [x] **15.1 Delegation return path via a distinct `agent-return` channel.** Two internal channels:
      **`agent`** (delegation, `platform_id`=target group) and **`agent-return`** (worker reply,
      `platform_id`=originating session id) — distinct so a forward's routing can't inherit a return
      `thread_id` via `resolve_address`'s field-by-field fallback. `delivery::delegate_to_agent`
      resolves/creates the worker session **namespaced by source** (`find_active(target, None,
      Some(source.id))`), writes the inbound (`source_session_id=source.id`), and sets the worker
      session's default routing to `{agent-return, platform_id:source.id}`. `return_to_session`
      resolves the originating session by id (`sessions::get`), writes the inbound there (trigger),
      and leaves its default routing (the user's chat) intact. Net effect: worker reply (empty
      routing → worker default = return address) → originating session → concierge re-runs → replies
      to the user; multi-hop relays up the chain. No new tables/adapters; the long-reserved
      `source_session_id` is now populated. Tests: forward (namespacing + return routing), return
      (lands in the user session, preserves its routing), full round-trip. ARCHITECTURE §10 updated.

## M16 — Agent activity monitor ("mission control")

A real-time view of what every agent is doing: a **board** (per-agent status), a **feed** (timeline),
and **sidebar presence** (busy dots), with phase + tool + the message being worked on, covering
active + queued + idle agents. Single-flight today (one run at a time + a queue); the design scales
to parallel runs for free. Reuses the M13 hub+SSE pattern and the M14 run-lifecycle hooks.

- [x] **16.1 Activity hub + supervisor reporting** (`src/activity.rs`, mirrors `logs.rs`). `ActivityHub`
      = `Mutex<HashMap<AgentGroupId, AgentActivity>>` + bounded `feed` ring + `broadcast::Sender`.
      `started/phase/finished/failed` update the map (finished is Running→Idle only, so a turn failure
      stays visible), push a feed event, broadcast; `snapshot()` + `subscribe()`. Supervisor gets
      `activity: Option<Arc<ActivityHub>>` + `with_activity`; `run_drain` builds a `RunContext` (agent,
      chat from `messaging_group_id`, message via `draft_prompt`, delegated-by via
      `batch[0].source_session_id`) and calls the hub at the M14 lifecycle points (start / `on_progress`
      phase / finish / `Err`). In-memory, `Option` so runs are unaffected. `app.rs` wires it +
      `WebState.{activity,queue}`. Tests: the state machine (lifecycle, cold failure, subscribe, cap).
- [x] **16.2 Admin activity board + feed + SSE.** `src/web/activity.rs`: `page` merges the hub snapshot
      with all agents (`agent_groups::list` → idle) and the queue (`RunQueue::snapshot()` → resolve to
      agents → "queued"); `stream` mirrors `logs::stream`. Routes `/admin/activity` + `/stream`;
      **activity** nav entry (+ `activity_active` on admin/tasks/logs structs). `templates/activity.html`
      board (status badge, chat + delegated-by, live phase, client-ticked elapsed, message snippet) over
      a feed; `claw.js` controller (card update-in-place + feed prepend + elapsed tick); `.act-*`
      tokens-only CSS (incl. mobile stack). Verified live over `/admin/activity/stream` (Andy
      running→idle, Coder failed); `screenshots/activity.png`. ARCHITECTURE §4 entry.
- [x] **16.3 Sidebar presence.** Extended `claw.js` `onRun`: a `working` event for any chat lights a
      pulsing dot on its sidebar `.chat-link`; `idle` clears it. `.chat-link--busy` CSS (pulse). Reuses
      M14's per-chat `run` SSE — no backend change.

## M17 — SMS channel via sim-server (connector abstraction)

A second real `ChannelAdapter`: SMS through **sim-server** (the separate Rust daemon driving a
Huawei E3372 dongle; authenticated HTTP API). The unit of assignment is the **connector**: one
`connectors` row = one external channel instance = one assigned agent group, managed as data so
the whole line moves agents in one admin edit — and so Telegram/WhatsApp later are just a new
`ConnectorKind` variant + adapter module (the exhaustive `match` in `build_adapters()` breaks
compilation until wired). Inbound = `GET /api/messages?after_seq=N` cursor polling (`Seq` is
persistent + monotonic → restart-safe by construction; at-least-once, dup/loss window = one
message) plus an HMAC-verified **webhook wake-up ping** for near-instant delivery — the cursor
stays the single source of truth (sim-server webhooks only cover messages noticed live and retry
twice). Outbound = `POST /api/messages {phone, content}` (server splits long texts; no MMS —
text-only). Amends the §1 non-goal (adapters whose transport is a plain HTTP API live in trunk).

- [x] **17.1 Connectors resource + routing fallback.** Migration `007-connectors`
      (`connectors(id, kind TEXT UNIQUE, label, config JSON, agent_group_id, enabled)`) +
      `src/db/connectors.rs`; `ConnectorKind` enum + per-kind typed config (SMS:
      `{base_url, token, webhook_secret?}`) in `protocol/entities.rs`; `connectors-*` CRUD in
      `src/commands/resources.rs` (the registry-rendered admin UI picks the forms up with zero
      `web/` changes). Router fallback: a messaging group with no wirings targets the enabled
      connector for its `channel_type` (DM always-engage, Shared session — wiring defaults);
      explicit wirings win. Tests: CRUD round-trip, fallback routes + enqueues, disabled
      connector still drops "no-wiring", explicit wiring beats fallback.
- [x] **17.2 Cursor store + SMS pure functions.** Migration `008-channel-cursors`
      (`channel_cursors(channel_type TEXT PRIMARY KEY, cursor INTEGER NOT NULL)`) +
      `src/db/channel_cursors.rs` (get/upsert). `src/channels/sms.rs` skeleton: serde types for
      sim-server payloads (`Index`/`Seq: Option<i64>`/`Smstat`/`Phone`/`Content`/`Date` renames)
      + pure fns — `to_inbound_event` (skips empty content; phone → `platform_id`;
      `is_group=false`), `advance_cursor` (never advances over `Seq: None`), `render_sms(kind,
      &OutboundContent, &[OutboundFile]) -> String` (chat passthrough; question text as-is —
      answers come back as plain inbound text, the open `questions` row expires via sweep TTL;
      approval → "approve in the web admin" — SMS sender identity is spoofable, not an
      authorization channel; files → "[N attachment(s) not deliverable over SMS]"). Tests:
      table-driven over all three + cursor round-trip.
- [x] **17.3 SmsChannel adapter + app wiring.** `run()`: first run (no cursor row) fetches
      `after_seq=0` and sets the cursor to max `Seq` **without routing** (no history flood);
      then ~2 s tick, per-message send-into-mpsc → persist cursor (at-least-once), exponential
      backoff 2 s→30 s on consecutive failures (401 once at ERROR), cancel-aware and
      `Notify`-wakeable. `deliver()`: `render_sms` → `POST /api/messages`, 10 s timeout, non-2xx →
      `ChannelError::Delivery` (delivery.rs's 3 attempts apply), returns `Ok(None)`. `app.rs`
      builds adapters from enabled connector rows (restart applies changes). Tests: in-process
      axum mock of both endpoints — first-run skip, poll → `InboundEvent` fields, cursor survives
      a simulated restart, send body + bearer header, 500 → `Delivery` error.
- [x] **17.4 Webhook wake-up + docs.** `POST /api/hooks/sms` outside the login gate: verify
      `X-Sms-Signature` = HMAC-SHA256(body, connector `webhook_secret`) constant-time; fire the
      shared `Notify` → immediate poll. 204 valid / 401 bad-or-missing signature / 404 when no
      SMS connector or secret is configured. New deps `hmac` + `sha2` (RustCrypto, pure Rust),
      pinned with §13 justification. Tests: valid signature triggers an immediate fetch on the
      mock, tampered body → 401, no secret → 404. Docs: ARCHITECTURE §10 delivery semantics
      (at-least-once inbound, hook-as-ping), §11 (LAN-trust link to sim-server, spoofable sender
      widens reach once assigned, HMAC hook surface), §12 config, §13 (`hmac`/`sha2`; SSE not
      worth the reqwest `stream` feature given hook+poll).
- [ ] **17.5 (optional) Connector reconciler.** Watch `connectors` rows and spawn/cancel adapter
      tasks live so admin edits don't need a daemon restart.

Backlog (unscheduled, from decision 001): claw-as-MCP-server as an additional control surface for
external agents; per-group MCP server configuration (M12 ships a single global server);
cross-turn tool-round persistence if 4.7's mitigation proves insufficient.
