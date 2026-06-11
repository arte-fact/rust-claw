# rust-claw

Personal AI assistant: one Rust daemon (`claw`), 100 % Rust — a built-in web UI routing messages
to an in-process agent loop (any OpenAI-compatible endpoint; chat and coder tool profiles), with
SQLite session DBs as the only IO surface. Design: [ARCHITECTURE.md](ARCHITECTURE.md).
Execution: [PLAN.md](PLAN.md). Removed-pi rationale: docs/decisions/001-drop-pi.md.

## Workflow

- Work proceeds through PLAN.md: **one session = one subtask**. Start from a green tree, finish
  with the subtask's deliverable done, all checks passing, and its checkbox ticked.
- If a subtask turns out too large for a session, split it in PLAN.md first, then do the first part.
- Architecture changes require updating ARCHITECTURE.md in the same session — the doc and the code
  must not drift.

## Checks (all must pass before a subtask is done)

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## UI verification — snapshot & debug (mandatory for UI subtasks)

Any subtask that touches the web UI (templates, `claw.css`, `claw.js` — M7.1, M7.3, M9.2, and any
later UI change) is not done until the rendered result has been **visually verified** in a real
browser AND the **`screenshots/` folder is refreshed**. Tests prove behavior; screenshots prove
the design rules (§9.1 Nord tokens, Fira Code, flat, no bubbles).

**Boot a throwaway instance** (background Bash):

```bash
TMPDATA=$(mktemp -d) CLAW_DATA_DIR=$TMPDATA CLAW_PORT=8471 CLAW_AUTH_TOKEN=ui-debug-token \
  ./target/debug/claw serve
# seed content over the API: curl -c jar -d 'token=…' /login, then POST /api/chats + messages
```

**Two browser tool sets are available — know which to use:**

- **Playwright MCP** (`mcp__plugin_playwright_playwright__browser_*`) — local browser. Reaches
  `127.0.0.1`, performs interactions (`browser_navigate`/`browser_type`/`browser_click` after
  `browser_snapshot`), **saves screenshots to files** (`browser_take_screenshot` with `filename`
  drops the PNG in the project root → move it into `screenshots/`), and surfaces **console
  errors** in its output — read them, they catch 404s/JS breakage. Use this for the screenshot
  archive and interaction debugging. `.playwright-mcp/` artifacts are gitignored.
- **Web Search Hacks MCP** (`screenshot`/`interact`/`fetch`) — remote headless browser. Returns
  images in-context only (cannot save files) and CANNOT reach `127.0.0.1`; use the LAN address
  (`hostname -I | awk '{print $1}'`). Fine for a quick look; not for the archive.

**The `screenshots/` folder is living documentation — keep it current.** One PNG per view, named
after the view (`login.png`, `chat.png`; later: `tasks.png`, `admin-<resource>.png`,
`question-card.png`). Whenever a UI subtask changes what a view looks like, recapture that view
(seeded with realistic content) and overwrite the file in the same session. Screenshots are
committed; a stale screenshot is a doc bug.

Iterate: check each capture against ARCHITECTURE.md §9.1 (palette roles, type, spacing, status
colors), fix and reshoot until it matches. Kill the server when done — verify with
`ss -tln | grep <port>`, not `pgrep -f` (which matches its own shell).

## Code rules

**Zero clippy warnings, no allow trick.** `-D warnings` is the bar. `#[allow(...)]`,
`#![allow(...)]`, and `#[expect(...)]` are forbidden — if clippy complains, restructure the code
until it doesn't. No `unsafe`.

**Names over comments.** Minimal comments, never narrative ("then we…", "this function does…").
Explicit function and variable names carry the meaning; a comment is allowed only for a
non-obvious constraint or invariant the code cannot express (e.g. why a pragma is load-bearing).
No doc-comment boilerplate that restates the signature.

**TDD, high coverage.** Write the failing test first, then the implementation. Table-driven tests
for decision logic. Extract pure functions from IO-bound code so the logic is testable without
tokio/DB scaffolding (watchdog decisions, backoff, engage evaluation, formatters). Every bug fix
starts with a regression test. Integration points get contract tests (see ARCHITECTURE.md §14 —
the session-DB schema lives once in `src/session/schema.sql`).

**Architecture first, small files.** Respect the module boundaries in ARCHITECTURE.md §4. Refactor
*as you go*: when a file approaches ~300 lines or a function ~40, split before adding more. New
responsibilities get new modules, not new sections in existing files. No god-files, no `utils.rs`
dumping ground.

**Idiomatic modern Rust (edition 2024).** Take advantage of the language:

- Newtypes for every id, enums over stringly-typed values, exhaustive `match` (no `_ =>` on
  domain enums — adding a variant must break compilation where it matters).
- Make illegal states unrepresentable; encode invariants in types (e.g. session-DB writer/reader
  handles) rather than in comments or runtime checks.
- `Result` everywhere; `thiserror` enums in modules, `anyhow` only at binary edges. `unwrap`/
  `expect` only in tests and provably-infallible spots (with the proof obvious at the call site).
- Zero-cost abstraction: generics where it's hot or static, `dyn` where it's a genuine plugin
  seam (`ChannelAdapter`, `AgentProvider`). Iterators and combinators where they read better than
  loops — not as dogma.
- Prefer compile-time work: askama templates, `rust-embed` assets, `const`/`static` tables,
  exhaustive serde derives — over runtime parsing and reflection-style maps.
- Ownership before `Rc`/`RefCell`; channels before shared mutexes; `Arc<Mutex<_>>` only where the
  architecture says shared state actually exists (RunQueue, adapter registry).

## Project specifics

- SQLite access is sync `rusqlite` behind `spawn_blocking` helpers — never block the runtime.
- Seq parity invariant: the host side writes even `seq` in `messages_in`, agent runs write odd in
  `messages_out` (§5). Don't touch this without reading ARCHITECTURE.md §5.
- CSS is design-token oriented: components consume semantic tokens only; literal colors/px/fonts
  exist solely in the `:root` block of `claw.css` (§9.1). Lint: grep `#`/`px` outside the tokens
  block must return nothing.
- The web UI has no build step: Askama templates + hand-written `claw.css`/`claw.js`. The project
  is 100 % Rust — do not introduce Node, TypeScript, or any second runtime (decision 001).
- Dependencies: check before adding (supply-chain), pin in `[workspace.dependencies]`-style table
  in Cargo.toml, prefer the already-chosen set (ARCHITECTURE.md §13). Adding a dependency is an
  architecture decision — justify it in the PR/commit message.
