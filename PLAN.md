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
- [ ] **4.7 Coding-group end-to-end.** Integration test: coder-profile group, mock LLM scripted
      to call `bash` + `edit` then reply; `#[ignore]` real-model test. Decide + document the
      cross-turn tool-memory approach (transcript currently persists chat only). Milestone exit:
      chat↔coder delegation via `send_to_agent`. (§8.4, §8.5)

## M5 — Scheduling + resilience

- [ ] **5.1 Sweep.** 60s task: due `process_after` → enqueue, due `deliver_after` → deliver,
      recurrence advance (croner, scheduled-time-based, `series_id`). Time-injected tests. (§10)
- [ ] **5.2 Task lifecycle.** `schedule_task` system action → `messages_in` insert;
      list/cancel/pause/resume/update actions; task formatting (`<task>`, pre-script,
      `wake_agent=false`). End-to-end schedule-fire-reply test. (§8.6)
- [ ] **5.3 Watchdog + recovery.** No-event watchdog (pure decision fn + kill), retry backoff
      (`5s·2^tries`, 5 → failed), startup `processing→pending` reset, circuit breaker file.
      Tests for every decision branch. (§8.2)

## M6 — Admin surface

- [ ] **6.1 Socket server + dispatch.** Frame codec, `UnixListener` (0600, stale unlink),
      dispatch with `CallerContext`, error codes. Temp-socket tests. (§10)
- [ ] **6.2 Command registry + CLI client.** `CommandDef` with arg display metadata (§9.2),
      generic CRUD generation per resource, `claw <resource> <verb>` subcommands + table output.
      Tests: registry → CLI → socket → dispatch round-trip.
- [ ] **6.3 Agent transport + gates.** `system`-row CLI transport, `cli_scope` enforcement
      (disabled/group/global, auto-fill own group), router command gate (pass/filter/deny against
      `user_roles`). Tests. (§10)

## M7 — Interactivity + approvals

- [ ] **7.1 ask_user_question end-to-end.** `pending_questions`, question card template + SSE,
      answer POST → resolution unblocking the native tool; card collapse on answer; timeout.
      Visual verification: snapshot the card pre/post answer. (§8.5, §9.1)
- [ ] **7.2 Approvals.** `Access::Approval` gating, `pick_approver` (owner for web-first),
      approval cards in the UI slot, allow/deny → action execution + `system` result row. Tests. (§10)
- [ ] **7.3 Web admin.** Registry-driven tables + forms (groups incl. provider/endpoint/model
      dropdowns, wirings, endpoints, users/roles), bespoke Tasks page (next fire via croner,
      pause/resume/cancel). Visual verification: snapshot each generated resource page. (§9.2)

## M8 — Packaging

- [ ] **8.1 Image + compose.** Multi-stage Dockerfile (rust builder → debian-slim + binary +
      CA certs; no Node), compose file with `/data` volume + env, first-run token
      generation/printout, `/data` bootstrap. Container smoke script. (§2)
- [ ] **8.2 Ops polish.** Migrations-on-boot upgrade path, engage modes in router (pattern/
      mention/mention-sticky — moved here from M2's always-engage), README quickstart, backup notes.

## M9 — Extended messaging + extensibility

- [ ] **9.1 create_agent.** System action + approval → group row + folder scaffold + default
      wiring; agent-initiated via tools. (§8.6)
- [ ] **9.2 Multi-chat UI.** Multiple web chats (create/archive), per-thread sessions surfaced in
      the UI, accumulate (`trigger=0`) policy support end-to-end. Visual verification: snapshot
      the chat list + thread views.

Backlog (unscheduled, from decision 001): MCP client in the native loop (per-group MCP servers as
the tool-extensibility seam, `rmcp`); claw-as-MCP-server as an additional control surface for
external agents; cross-turn tool-round persistence if 4.7's mitigation proves insufficient.
