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
- [ ] **12.3 Dockerfile + docs.** Builder stage clones + `cargo build --release`s the stdio binary,
      copies `mcp-web-search-stdio` into the runtime image; runtime stage apt-installs `chromium` +
      shared libs and sets the Chrome-path env the binary expects. Enabled by default (no env to add).
      Update README (new web tools + Chromium footprint) and ARCHITECTURE (§4 `mcp/` module + a note
      on the Chromium image-size deviation from decision 001).

Backlog (unscheduled, from decision 001): claw-as-MCP-server as an additional control surface for
external agents; per-group MCP server configuration (M12 ships a single global server);
cross-turn tool-round persistence if 4.7's mitigation proves insufficient.
