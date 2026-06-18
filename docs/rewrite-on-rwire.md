# Implementing claw on rwire

> Companion to `SPEC.md`. The spec says *what to build*; this says *how to build it
> efficiently on rwire* (`/home/artefact/rwire`), using the patterns proven by the
> worked example `/home/artefact/llama-modnitor-rwire`. Verified against rwire source,
> not docs alone.

---

## 0. The one insight everything follows from

**claw and rwire are the same architecture.** claw already is: *the server owns all
state; background loops mutate it; the surface reflects the state.* rwire **is** that,
as a framework:

| claw (SPEC) | rwire |
|---|---|
| DB is the message bus; pipeline loops mutate it | `#[storage(shared)]` state; background threads mutate via `update_shared` |
| "only the chat that changed should update" (hand-coded per-chat SSE) | `update_shared_changed(ChangeSet)` — fine-grained re-render, automatic |
| web channel = SSE hub + Askama templates + `claw.js` + `web_messages` ledger | server renderers over shared state; ~1.5 KB generic client; binary wire |
| activity hub + `WebNotifier` + activity SSE stream | background writes to shared fields; renderers re-render |
| SQLite everywhere | `#[storage(persisted)]` is rusqlite; the pipeline keeps its own rusqlite |

So the rewrite is not "port claw to a web framework." It is "**delete claw's entire
hand-rolled presentation/transport layer** and let rwire be it." That deletion is the
efficiency win (§7).

---

## 1. The load-bearing facts about rwire

1. **Runtime: `async-std` + `async-tungstenite` (WebSocket).** *Not tokio.*
2. **Handlers are synchronous:** `#[handler] fn f(state: &mut S, ctx: &EventContext)`.
   Macros auto-derive a `ChangeSet` of the fields the handler mutates. **No async work
   in a handler** — it must mutate state and return fast.
3. **Background → UI push:** a thread holding `Arc<SharedServerState>` calls
   `shared.update_shared_changed::<App>(ChangeSet::from_fields(&[App::FIELD_X]), |a| …)`;
   every connected client re-renders exactly the renderers that read field `X`.
4. **Storage:** `#[storage(memory)]` (per-connection), `#[storage(persisted)]`
   (rusqlite, per-session), `#[storage(shared)]` (one global instance, all clients).
5. **UI = pure renderers + fluent builder + utility tokens:**
   `#[renderer] fn v(s: &App) -> ElementBuilder`; `el(El::Div).st([St::FlexCol]).on(Ev::Click, h())`.
6. **Server:** `Server::bind(addr)?.root(ui::app).with_shared_state(shared).theme(t).routes(r).run().await`.
7. **No build step, no JS source, no CSS files.** One Rust binary.

---

## 2. The architectural seam: pipeline as a tokio subsystem behind a channel

The only real impedance: claw's pipeline is **tokio + async + heavy IO** (inference
HTTP, channel adapters, per-session SQLite, MCP child). rwire is **async-std + sync
handlers**. **Don't reconcile the runtimes — isolate them.** This is exactly what the
llama-monitor example does with its process supervisor (`OnceLock<Sender<Msg>>` +
a dedicated thread + `update_shared`); claw does the same at larger scale.

```
        rwire (async-std)                          pipeline (tokio, own runtime, own thread)
   ┌───────────────────────────┐   Command     ┌──────────────────────────────────────────┐
   │ #[handler] (sync, thin)   │──channel────▶ │ router · queue · supervisor · delivery ·  │
   │   post Command, mutate     │               │ sweep · channel adapters · providers ·    │
   │   per-connection UI state  │               │ MCP · central.db · per-session DBs        │
   │ #[renderer] (pure)         │◀──────────────│ writes read-models via                    │
   │   read shared state         │ update_shared │ Arc<SharedServerState>.update_shared_…    │
   └───────────────────────────┘               └──────────────────────────────────────────┘
```

- **Boot:** the composition root creates `Arc<SharedServerState>` (rwire) and a tokio
  runtime on a dedicated thread; hands the Arc to the pipeline; registers a global
  `OnceLock<Sender<Command>>` for handlers; then `Server::…run().await`.
- **Inbound from the UI:** a handler posts `Command::Submit{ session, text }` (or
  `Command::Admin{…}`, `Command::AnswerQuestion{…}`) — returns immediately.
- **Pipeline does the async work** (route → run → reply) entirely inside its own
  runtime, against the unchanged §5–§9 contracts.
- **Outbound to the UI:** the pipeline's *web surface* is no longer a `ChannelAdapter`
  emitting SSE — it's a **projection** that writes the new message into the shared
  `App` and calls `update_shared_changed`. The transcript renderer reacts.

**Consequence:** the entire SPEC §5–§9 core (per-session ledgers, seq parity, engage,
scheduling, delivery, connectors, providers, MCP) lives **untouched** inside the tokio
subsystem. rwire only replaces SPEC §10 (the web surface) and absorbs §13's activity
broadcast.

---

## 3. State decomposition (SPEC → rwire state)

Keep **one system of record**: claw's `central.db` + per-session DBs, owned by the
pipeline. rwire shared state is a **read-model projection** of it for the UI — never a
second source of truth. (The pipeline owns process/connection handles and DB writers,
which aren't serializable into shared state anyway — same reason llama-monitor keeps its
`Child` handles in the supervisor thread, not in `App`.)

```rust
#[derive(State, Default)]
#[storage(shared)]                 // one instance, all clients, mutated by the pipeline
struct App {
    chats:      Vec<ChatSummary>,  // sidebar: id, title, last activity, busy dot
    open:       Option<Transcript>,// the *currently open* chat only (not every transcript)
    activity:   Vec<AgentActivity>,// mission-control board
    logs:       Vec<LogLine>,      // capped ring, appended by the tracing layer
    // admin read-models, projected from central.db:
    endpoints:  Vec<EndpointRow>,
    agents:     Vec<AgentRow>,
    connectors: Vec<ConnectorRow>,
    wirings:    Vec<WiringRow>,
    questions:  Vec<OpenQuestion>, // pending question/approval cards
}

#[derive(State, Default)]
#[storage(memory)]                 // per-connection UI ephemera; never broadcast
struct Ui {
    open_chat:  Option<SessionId>, // which chat this connection is viewing
    draft:      String,            // composer text (stable input id → cursor preserved)
    admin_tab:  String,
    editing:    Option<String>,    // which admin row is expanded
}
```

Projection rule: when the pipeline writes a session DB row, it updates the matching
shared field with a **precise** `ChangeSet` so unrelated regions don't re-render —
e.g. a new message in the open chat → `FIELD_OPEN`; a busy-state change → `FIELD_CHATS`
(the dot) without touching `FIELD_OPEN`. This is the llama-monitor "broadcast only
`FIELD_HW`, leave the form alone" discipline, applied to chat.

> Do **not** put every session's full transcript in shared state. Project only the open
> chat (per connection's `open_chat` selects which session the pipeline materializes).
> Switching chats posts `Command::OpenChat{session}` → pipeline loads that transcript
> into `App.open` → renderer reacts.

---

## 4. The chat surface (replaces SPEC §10 web channel + SSE + claw.js + web_messages)

```rust
#[renderer]
fn chat_view(app: &App, ui: &Ui) -> ElementBuilder {
    el(El::Div).st([St::DisplayFlex]).append([
        sidebar(app, ui),     // reads app.chats
        thread(app),          // reads app.open  (the transcript)
        composer(ui),         // reads ui.draft
    ])
}

#[renderer]
fn thread(app: &App) -> ElementBuilder {
    let Some(t) = &app.open else { return el(El::Div) };
    el(El::Div).st([St::FlexCol, St::GapSm]).append(
        t.messages.iter().map(|m| bubble(m))   // user/assistant/question card
    )
}

#[handler]
fn send(ui: &mut Ui, ctx: &EventContext) {        // sync, thin
    let text = std::mem::take(&mut ui.draft);
    if let Some(s) = &ui.open_chat {
        pipeline::post(Command::Submit { session: s.clone(), text }); // → tokio subsystem
    }
}

```

End-to-end: `send` posts a command → router writes inbound (even seq) → supervisor runs
the agent → provider writes outbound (odd seq) → delivery's **web projection** appends
the message to `App.open` and calls `update_shared_changed(FIELD_OPEN)` → `thread`
re-renders for every client viewing that chat. **Gone:** `web_messages` table, the SSE
hub, `WebNotifier`, `claw.js`, the per-chat SSE `run`/`working` events (the busy dot is
just an `App.chats[i].busy` field the supervisor flips), and the templates.

Questions/approvals are just cards in the transcript whose buttons post
`Command::AnswerQuestion{id, choice}` — no separate answer endpoint.

### 4.1 Inputs, debounce & forms — one reusable pattern (used app-wide)

The composer above does **not** sync the draft per keystroke — and that's deliberate.
How rwire handles text input governs both the chat box and every admin field (§5), so
fix the pattern once.

**What rwire gives you:**
- Plain `.on(Ev::Input, h)` is **already debounced ~250 ms client-side** (the runtime's
  `snd()` only debounces `input`-type events). `Ev::KeyDown`/`KeyUp`/`Change` fire
  immediately — a common reason a chat box "feels un-debounced" is it's bound to keydown.
- Explicit, tunable debounce on **any** element: `on_debounced(ev, handler, delay_ms)`;
  on the `Input` component: `Input::on_input_debounced(handler, delay_ms)`; or set it on
  the handler itself: `handler.debounce(ms)` (preserved through `on_ref`). All are
  **client-side** — keystrokes don't reach the server until the user pauses.
- Inputs are effectively **uncontrolled** once typed: rwire writes the `value`
  *attribute* via `setAttribute`, which the browser ignores after user input. So a
  server-rendered `value=""` does **not** reliably clear a box — design around it.

**The rule — two cases:**

1. **Submit-style fields (chat composer, "add" forms): read on send, don't sync.**
   Wrap in `<form>`; name the input; the submit handler reads `ctx.field("…")`. **Zero
   server traffic while typing**, one round-trip on submit, and Enter submits natively:
   ```rust
   el(El::Form).on(Ev::Submit, send())                 // fires on Enter or the button
       .append([
           Textarea::new().id("composer").name("text").build(),
           Button::primary("Send").build().attr("type", "submit"),
       ])
   #[handler] fn send(ui: &mut Ui, ctx: &EventContext) {
       let text = ctx.field("text").unwrap_or_default().trim().to_owned();
       if !text.is_empty() { if let Some(s)=&ui.open_chat { pipeline::post(Command::Submit{session:s.clone(), text}); } }
   }   // no draft field, no per-keystroke handler
   ```
   This is the framework-blessed pattern (the rwire `todo` example). Use it for the
   composer and every admin "create" row.

2. **Live-reacting fields (search box, a draft the server must echo): debounce-sync.**
   Only when the server genuinely needs the value mid-typing (filtering a list, a char
   counter, a slash-command preview). Then sync, but **always debounced and into a
   narrow field** — never a broad one. Provide one app-wide helper:
   ```rust
   pub const INPUT_DEBOUNCE_MS: u16 = 350;
   pub fn deb(h: HandlerSpec) -> HandlerSpec { h.debounce(INPUT_DEBOUNCE_MS) }
   // …Input::text().id("q").build().on(Ev::Input, deb(filter_changed()))
   ```

**The churn trap (the real cause of "laggy typing"):** a handler that mutates a *broad*
shared field re-renders every renderer that reads it. If the chat draft or an admin
field lives in `App.nodes`/`App.chats`, each (even debounced) keystroke re-renders the
whole list. Keep per-keystroke drafts in **`#[storage(memory)]` `Ui`** (per-connection,
not broadcast) or use case 1 (no sync at all). Reserve broad-field writes for actual
domain changes the pipeline projects.

> **rwire has no chat-input component** (it has `Input`, with a `search()` variant, and
> `Textarea` — neither debounced by default). Compose `Textarea` + submit `Button` into
> a small app-level `chat_input()` helper, and use `deb()` for the rare case-2 fields.

---

## 5. Admin CRUD (the "registry renders itself" idea, done better)

The command registry stays the single source of truth (SPEC §11). But instead of
generating HTML forms from arg specs, admin is **renderers over the shared read-models**
with **inline-editable rows**, exactly like llama-monitor's catalog-driven flag forms:

```rust
#[renderer]
fn endpoints_admin(app: &App, ui: &Ui) -> ElementBuilder {
    Stack::column().children(
        app.endpoints.iter_with_ref().map(|(r, e)| editable_row(e, r, ui))
    ).build()
}

#[handler]
fn save_endpoint(_ui: &mut Ui, ctx: &EventContext) {
    // gather row fields from ctx, post to the pipeline; the registry validates + writes
    pipeline::post(Command::Admin { cmd: "endpoints-update".into(), args: ctx.form() });
}
```

The pipeline runs the actual `endpoints-update` command (same handler the CLI uses),
writes `central.db`, then re-projects `App.endpoints`. Forms get focus/cursor
preservation and ItemRef safety for free. The CLI (`claw <resource> <verb>`) still
drives the identical registry over the unix socket — **one registry, two surfaces**, but
now the web surface is ~50 lines of renderers instead of a templating subsystem.

---

## 6. Live dashboards (activity board, logs) — the llama-monitor pattern verbatim

These are *exactly* what rwire+llama-monitor already do well:
- **Activity board:** the supervisor's lifecycle hooks (started/phase/finished/failed)
  write `App.activity` with `ChangeSet::from_fields(&[App::FIELD_ACTIVITY])`. The board
  renderer reflects it. No activity SSE stream, no hub/subscribe.
- **Logs viewer:** the tracing layer pushes capped lines into `App.logs` (a shared Vec)
  and broadcasts `FIELD_LOGS`. No log SSE.
- **Sidebar presence (busy dots):** a `busy: bool` per chat summary, flipped by the
  supervisor at run start/end. Pure field reactivity; no per-chat events.

---

## 7. What gets deleted (the efficiency, concretely)

Relative to the current build, an rwire implementation **removes**:
- `templates/` (Askama) and the dual "full-page + SSE-fragment" rendering path.
- `assets/claw.css` hand-maintained design-token system → use rwire `St` tokens + a Nord
  `Theme`.
- `assets/claw.js` and `rust-embed` of UI assets → the generic ~1.5 KB client is the
  framework's.
- `web/sse.rs` (the hub), `web/notify.rs` (WebNotifier), the activity/logs SSE streams.
- `db/web_messages.rs` + the `web_messages` table → the transcript is the session DB,
  projected.
- The web `ChannelAdapter` → replaced by the command-channel bridge + projections.
- Most of `web/` (auth middleware aside) → a handful of renderers + thin handlers.

What's gained: fine-grained updates for free, tiny wire payloads, no frontend build, one
rendering path, and the disappearance of the "rich web vs degraded channel" asymmetry —
the web surface is just shared state, every channel is equal again.

---

## 8. Proposed module layout (rwire-flavored SPEC §19)

```
crates (or modules):
  domain/        pure core — UNCHANGED from SPEC §19 (ids, entities, message, decisions)
  pipeline/      the tokio subsystem — the SPEC §5–§9 core, runtime-isolated
    ports, store(sqlite), inference, mcp, providers, channels(sms,…),
    router·queue·supervisor·delivery·sweep, commands(registry)
    └─ exposes: enum Command (Submit, OpenChat, Admin, AnswerQuestion, …)
                and a Projector that writes read-models into rwire shared state
  ui/            rwire surface (async-std side)
    state.rs     App (#[storage(shared)]) + Ui (#[storage(memory)])
    app.rs       root component + Router pages + auth gate
    chat.rs admin.rs activity.rs logs.rs    (#[renderer]s)
    handlers/    thin #[handler]s → pipeline::post(Command)
  bridge.rs      the seam: spawns the tokio runtime + pipeline thread, holds the
                 Arc<SharedServerState>, registers OnceLock<Sender<Command>>,
                 implements Projector (pipeline → update_shared_changed)
  app/main.rs    Server::bind().with_shared_state(shared).root(ui::app).theme(nord).run()
                 + bridge::spawn_pipeline(shared.clone())
```

`domain/` and `pipeline/` are reusable and **testable without rwire** (and without a
browser): the SPEC's ports-and-adapters discipline means the UI is just another adapter.
`ui/` is small and declarative.

---

## 9. Integration points to design up front (the real risks)

1. **Plain HTTP endpoints beside the WS server.** The **SMS wake-up webhook**
   (`POST /api/hooks/sms`) and `healthz` are *not* rwire pages — they're raw HTTP. rwire
   sits on async-std/tungstenite; confirm whether its `Router` can carry non-UI HTTP
   handlers, or run these on a **separate listener owned by the pipeline** (cleaner: the
   webhook belongs to the SMS subsystem anyway). Decide before committing.
2. **Auth on connections.** Map claw's single-token login to a gate on the rwire
   connection (login page → cookie/session → guard). Verify rwire's session/auth
   primitives (it has per-connection `session_id`); the admin must be gated.
3. **async-std ↔ tokio.** The pipeline runs its own tokio runtime on a dedicated thread;
   never call across runtimes directly — only the `Command` channel and `update_shared`.
   (Validated: llama-monitor crosses sync-thread ↔ shared-state this way.)
4. **Shared-state volume.** Project only the open chat's transcript + capped logs/feed;
   never the whole world. Large/lazy data stays in the DBs, fetched on demand into a
   per-connection view.
5. **rwire write-side maturity.** Per llama-monitor's plan, interactive *write* actions
   are the framework's newest surface; claw is write-heavy (composer, admin, answers).
   Build a vertical slice first (one chat: submit → echo provider → reply renders) to
   de-risk the handler→command→projection→re-render loop before porting the rest.

---

## 10. Recommended first vertical slice

To prove the whole seam in a day, in order:
1. `Server::bind` + a root with a hard-coded chat list and an empty thread (rendering).
2. `Ui.draft` + composer with `draft_changed`/`send` handlers (interactivity, focus).
3. The bridge: a tokio thread, a `Command::Submit` channel, the **echo provider** writing
   to a real per-session DB.
4. A `Projector` that, on echo reply, appends to `App.open` and
   `update_shared_changed(FIELD_OPEN)` (the round-trip).
5. Swap echo → native provider against a live endpoint.

If that loop is crisp, everything else (admin, activity, SMS, scheduling) is the same
shape: a `Command`, a pipeline that already works, and a projection back into shared
state.

---

## 11. Data structures & persistence with rwire's state system

This expands §3 into a decision and a concrete data model. The question is: *how much
of claw's data should rwire's state system own?*

### 11.1 What rwire's persistence actually is

- `#[storage(persisted)]` → **rusqlite**, but a **struct-snapshot store**: the State
  struct is serialized and keyed by `("{table}:{session_id}")`, flushed by rwire's
  background task (~100 ms). It is **per-session document storage**, not a relational
  backend — no joins, no foreign keys, no cross-row queries, no transactional
  multi-table writes.
- `#[storage(shared)]` → one global instance in **RAM** (not persisted; rebuilt on boot).
- `#[storage(memory)]` → per-connection **RAM**.
- All three are driven by **rwire's async-std runtime + flush task**. The claw pipeline
  runs on its **own tokio runtime, on another thread** (§2) — it cannot ride rwire's
  persistence, and channel adapters (SMS poll, etc.) write inbound **with no rwire
  connection at all**.

### 11.2 The decision: claw keeps its own system of record; rwire state is a projection

**Domain data stays in claw's own rusqlite, owned by the pipeline.** rwire's state system
owns only UI-shaped data. Four reasons it must be this way:

1. **Relational integrity.** Routing resolves *messaging group → wirings → agent group*;
   connectors, endpoints, sessions are FK-linked with migrations. rwire-persisted is
   struct-per-key — it can't express joins or constraints.
2. **The ledger contracts.** `messages_in/out` need seq parity, a status state machine,
   due-filtering, at-least-once, and crash recovery (`processing→pending`). Those are
   transactional SQL the pipeline must control — not background struct-flushing.
3. **Wrong runtime / wrong writer.** The pipeline (tokio) and the SMS adapter write
   domain rows outside any rwire connection. rwire-persisted state is mutated through
   handlers/`update_shared` on its own runtime.
4. **Source-of-truth discipline.** Two writers to the same data is the bug factory.
   One system of record (the pipeline's DBs); everything the UI shows is **derived**.

### 11.3 The mapping (what lives where)

| Data | Home | Persisted? | Who writes |
|---|---|---|---|
| Entities (endpoints, agents, messaging groups, wirings, connectors, sessions, users) | claw `central.db` (pipeline) | yes (rusqlite) | pipeline command handlers |
| Message ledgers (`messages_in/out`, delivered, routing, cursors) | per-session DBs (pipeline) | yes (rusqlite) | pipeline (router/provider/delivery/sweep) + channel adapters |
| Chat list, open transcript, activity feed, admin lists, logs ring | rwire `#[storage(shared)]` `App` | **no** (RAM; rebuilt on boot) | pipeline **projections** via `update_shared_changed` |
| Open-chat selection, draft (if synced), expanded admin row | rwire `#[storage(memory)]` `Ui` | no | UI handlers |
| Per-user UI prefs: theme, default chat, sidebar collapsed | rwire `#[storage(persisted)]` `Prefs` | **yes** (rwire's store) | UI handlers |

So **two SQLite worlds, cleanly separated**: the pipeline's (domain, the system of
record) and rwire's (only small per-connection UI preferences). They never overlap.

### 11.4 Data structures: domain types vs. read-models

Keep the two explicitly distinct (this is just SPEC §19's `domain` vs. the UI adapter):

- **Domain types** (in `domain/`, the system of record's shape) — the SPEC §3 entities
  and `message` content. Rich, FK-aware, used by the pipeline + DB layer.
- **Read-models** (in `ui/state.rs`) — flat, display-shaped projections with only what a
  renderer needs. Example for chat:

```rust
// domain (pipeline) — authoritative, in central.db / session DB
struct Session { id: SessionId, agent_group: AgentGroupId, messaging_group: MessagingGroupId, … }
struct OutboundMessage { seq: Seq, kind: MessageKind, content: OutboundContent, status: …, … }

// read-model (rwire shared App) — derived, display-only
struct ChatSummary { id: SessionId, title: String, last: String, busy: bool, unread: u16 }
struct Transcript  { session: SessionId, messages: Vec<Bubble> }
struct Bubble      { role: Role, text: String, card: Option<QuestionCard> }   // no seq/routing/status
```

The pipeline owns a small **projector** that maps domain rows → read-models. Renderers
never see a `Seq`, a routing field, or a DB id they don't need.

### 11.5 The projection mechanism (keep them in sync)

- **On boot:** the pipeline reads its DBs and fills `App` once (chat list, activity from
  the live run state, admin lists). `App` is RAM, so this rehydration on every start is
  the *only* "persistence" `App` needs.
- **On every domain write:** the pipeline calls the projector, which mutates the matching
  `App` field with a **narrow `ChangeSet`** (§3): a new outbound in the open chat →
  `FIELD_OPEN`; a status/busy flip → `FIELD_CHATS`; an admin edit → `FIELD_ENDPOINTS`.
  Precision here is what keeps typing/scrolling smooth (the §4.1 churn trap, generalized).
- **On `OpenChat`:** the pipeline materializes that session's transcript into `App.open`
  (and stops materializing the previous one) — only the visible transcript is ever in RAM.

### 11.6 What you do *not* do

- **Don't** model entities or ledgers as `#[storage(persisted)]` structs — you'd lose
  joins, constraints, the seq/status contracts, and you'd fight the runtime boundary.
- **Don't** let a renderer read domain types directly or hold a DB handle — renderers are
  pure functions over read-models; all IO is the pipeline's.
- **Don't** persist `App` (shared) — it's derived; rebuild it from the DBs on boot. The
  *only* rwire-persisted thing is the tiny per-connection `Prefs`.

### 11.7 Orthogonal: claw's two-DB split is not an rwire concern

Whether the rewrite keeps *central.db + one DB per session* or collapses to a single DB
(a SPEC §16 "reconsider") is independent of rwire — it's a pipeline-internal choice. rwire
sees neither; it only ever sees the projected read-models. Decide it on the pipeline's own
merits (isolation, file count, backup), not because of the UI framework.

---

## 12. rwire component map (`rwire-components` — ~50 components)

The library covers most of claw's UI; we compose a few missing pieces. Components take
handlers either as a convenience (`Button::primary("Save").on_click(save())`) or
explicitly (`.build().on(Ev::Click, save())`).

### 12.1 The core kit (≈15 components do ~90% of claw)

`AppShell` · `Stack` · `Card` · `Container` · `Grid` · `Button` · `Input` · `Textarea` ·
`Select` · `FormField` · `Table` · `Modal` · `Badge` · `Text` · `Alert`. Learn these first.

### 12.2 Per-surface mapping

| claw surface | rwire components |
|---|---|
| **App layout** | `AppShell` (`.header`/`.sidebar`/`.main`) — sticky header + chat sidebar + scrolling main |
| **Top nav** | `NavMenu` + `NavItem` (Chat / Admin / Activity / Logs); `ThemeToggle`; `Avatar`+`Badge` |
| **Login** | `Card` > `FormField` + `Input::password()` + `Button::primary`; `Alert` for bad token |
| **Chat sidebar (list)** | `Stack(col)` + `Input::search()` + `List`/`ListItem` (or styled rows); `Badge` = unread; busy dot = `Badge`/styled span |
| **Chat thread** | `Stack(col)` of message `Card`s + `Avatar` + the **markdown renderer** (§12.4); `Spinner` while the agent runs; `EmptyState` for a new chat |
| **Chat composer** | `<form>` + `Textarea` + `Button::primary("Send")` (read-on-send, §4.1) |
| **Question / approval card** | `Card` + `Text` + `Button`s (each posts `Command::AnswerQuestion`); `Alert` for approvals |
| **Admin resource list** | `Card` > `Table` (`.headers`, `.row`) + `Pagination`; per-row `DropdownMenu` (Edit/Delete) or inline-edit rows; `Badge` = status |
| **Admin create/edit** | `Modal` or `Drawer` > `FormField` × N (wrapping `Input`/`Select`/`Switch`/`Checkbox`/`Slider`) + `Button`s |
| **Delete confirm** | `Modal` + `Alert::warning` + `Button::destructive` |
| **Activity board** | `Grid`/`Stack` of `Card`s, `Badge` = phase, `Stat` = counts, `Spinner` = running |
| **Logs viewer** | `Table` (or `Timeline`) + `Code::block` for JSON lines; `Tag`/`Badge` = level |
| **Dashboard / KPIs** | `Grid` > `Stat` (`.trend`), `Progress` (e.g. context usage), `Alert` for health |
| **Notifications** | `Toast` + `ToastContainer` ("message sent", "connection lost") |

### 12.3 rwire-specific patterns to internalize

- **Overlays are state-driven, not imperatively shown.** `Modal`/`Drawer`/`DropdownMenu`
  always render their DOM; visibility is a field: `Modal::new().open(ui.modal_open)…`. To
  open/close, a handler flips a `bool` in `#[storage(memory)] Ui`; no show()/hide() calls.
- **`FormField` auto-wires label↔input ids** — just `.label("Endpoint").input(Input::text()…)`,
  add `.error(msg)`/`.required(true)` for validation state (pairs with `Input::invalid`).
- **Tables are values, not callbacks:** `Table::new().headers([…]).row(TableRow::new().cells([…]))`
  — build rows by iterating a read-model `Vec` with `iter_with_ref()` so row actions carry
  an `ItemRef` (type-safe index back to the handler).
- **`Stat`/`Progress`/`Badge`/`Timeline`** are pure presentational — feed them projected
  read-model values; they have no IO.
- Debounce/inputs: see §4.1 (the component path is `Input::on_input_debounced(h, ms)`).

### 12.4 Not provided — compose these (the real gaps)

1. **No chat-input component.** Compose `Textarea` + `Button` inside a `<form>` (§4.1);
   wrap once as an app-level `chat_input()`.
2. **No markdown renderer — this is the notable gap.** Agent replies are markdown
   (claw renders them server-side today). rwire gives `Code::block`/`Code::inline` but
   nothing for headings/lists/bold/links. Build a small **`markdown → ElementBuilder`**
   function in `ui/` (parse with a pure-Rust markdown crate; emit `Text`/`El::P`/`List`/
   `Code::block`/`Link`). It belongs in `domain`-adjacent UI code, is pure, and is unit-
   testable. Plan for it explicitly — it's load-bearing for the chat thread.
3. **No chat "bubble"** — it's just a `Card` + `Avatar` + the markdown render; make a
   `bubble(msg)` helper.
4. **Live-updating relative timestamps** ("2m ago") — `Timeline` shows a static `.time()`
   string; if you want it to tick, the projector updates the field, or accept static.

### 12.5 Components claw won't need

`Breadcrumb`, `Stepper`, `AvatarGroup`, `Image`, `Blockquote`, `Kbd`, `Footer`,
`Accordion`, `Slider` (unless a numeric setting wants it), `CopyButton` (handy for
showing a connector token once). Skip until a concrete need appears.
