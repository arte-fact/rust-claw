# claw

**Your own AI assistant, self-hosted as a single small daemon.**

claw is a personal AI assistant you run yourself. It's one Rust program with a
built-in web chat UI. You point it at any OpenAI-compatible model (a hosted one
like OpenRouter, or a local one like Ollama / llama.cpp / vLLM), and you get a
private assistant that can hold conversations, run scheduled tasks, ask you
questions, work with files and a shell, and even spin up specialised sub-agents —
all behind a login, with everything stored on one folder you control.

No accounts, no cloud dependency (beyond the model endpoint you choose), no
second runtime. It's 100 % Rust in one container.

---

## Table of contents

- [What you get](#what-you-get)
- [Quickstart](#quickstart)
- [First login](#first-login)
- [Make it smart: connect a model](#make-it-smart-connect-a-model)
- [Using claw](#using-claw)
  - [Chatting](#chatting)
  - [Multiple chats & agents](#multiple-chats--agents)
  - [Chat vs coder agents](#chat-vs-coder-agents)
  - [Scheduled tasks & reminders](#scheduled-tasks--reminders)
  - [Questions & approvals](#questions--approvals)
- [The admin panel](#the-admin-panel)
- [Configuration reference](#configuration-reference)
- [Your data & backups](#your-data--backups)
- [Upgrading](#upgrading)
- [Security notes](#security-notes)
- [Troubleshooting](#troubleshooting)
- [Command-line admin](#command-line-admin)
- [For developers](#for-developers)

---

## What you get

- **A private web chat** — clean, dark, keyboard-friendly, served by the app itself.
- **Any model you like** — bring a hosted OpenAI-compatible endpoint or a local
  one. Different agents can use different models.
- **Tools** — agents can send you messages, ask multiple-choice questions, and
  schedule work. "Coder" agents can additionally run shell commands and read /
  write / edit files in their own workspace.
- **Scheduling** — ask for a reminder or a recurring task ("every weekday at 9am…")
  and claw runs it on time, no separate cron.
- **Multiple agents** — keep a fast conversational assistant and a heavier coding
  assistant side by side; they can delegate to each other.
- **You stay in control** — sensitive actions (like an agent deleting config or
  creating another agent) pop an **Allow / Deny** card in your chat. Everything
  lives in one `/data` folder you can back up.

---

## Quickstart

You need **Docker** with the Compose plugin. From the project folder:

```bash
cp .env.example .env      # your config — edit if you like
docker compose up --build -d
```

That builds the image and starts claw. Then open **<http://localhost:8080>**
(or whatever `CLAW_PORT` you set).

> All configuration lives in **`.env`** (gitignored). Compose reads it
> automatically; `.env.example` documents every variable.

To stop it: `docker compose down` (your data survives in the volume).

## First login

claw is protected by a single login token.

- If you set `CLAW_AUTH_TOKEN` in `.env`, log in
  with that value.
- If you leave it unset, claw **generates one on first start and prints it to the
  logs**, then saves it so it stays the same across restarts:

  ```bash
  docker compose logs claw | grep "generated and saved a login token"
  ```

Paste the token into the login page. You'll get a chat, but it won't answer
intelligently yet — first connect a model.

## Make it smart: connect a model

Out of the box claw has no model configured, so it just echoes. To get real
answers, give it an **endpoint** and tell a **group** (agent) to use it. All of
this is in the web UI — click **⚙ admin** in the sidebar.

1. **Add an endpoint** — go to **admin → endpoints**, fill the `endpoints-create`
   form:
   - *Name*: a label, e.g. `openrouter`.
   - *Base URL*: the OpenAI-compatible base, e.g.
     - OpenRouter: `https://openrouter.ai/api/v1`
     - Local Ollama: `http://localhost:11434/v1`
     - llama.cpp / vLLM: `http://localhost:8000/v1`
   - *API key*: your key if the endpoint needs one (local servers usually don't).
     You can also point at an env var with *API key env*.

2. **Wire it to an agent** — go to **admin → groups**. The default agent is
   "Andy". In the `groups-update` form set:
   - *Group id*: copy it from the table at the top of the page.
   - *Provider*: `native` (the real model loop; `echo` is the no-model default).
   - *Endpoint*: pick the one you just added (it's a live dropdown).
   - *Model*: the model name your endpoint expects, e.g.
     `google/gemma-2-9b-it` or `qwen2.5:7b`.

3. **Chat.** Go back to a chat and say hello — you're now talking to your model.

You can repeat this with several endpoints and several agents (e.g. a fast chat
model and a heavier coding model).

## Using claw

### Chatting

Type in the composer, **Enter** sends (Shift+Enter for a newline). Replies stream
in live; a "working…" indicator shows while the agent is busy. Markdown in
replies is rendered. Your whole history is kept and reloads with the page.

### Multiple chats & agents

- **New chat** — use **+ new chat** in the sidebar. It's wired to your default
  agent.
- **A whole new agent** — go to **admin → groups** and use `groups-create`
  (name + optional provider/endpoint/model). This makes a new agent **and its own
  chat**, so it shows up in the sidebar immediately. Configure its endpoint/model
  the same way as above.
- **Archive a chat** — open it and click **archive** in the header. Archived chats
  move to a collapsible **"archived"** section at the bottom of the sidebar (their
  history is kept); open one and click **unarchive** to bring it back.

### Chat vs coder agents

Each agent has a **tool profile**:

- **chat** (default, safe) — messaging and scheduling tools only.
- **coder** — additionally gets a **shell** and **file** tools (read / write /
  edit) scoped to that agent's own working folder. Use this for an assistant that
  builds or edits things.

Set it in **admin → groups** (`groups-update` → *Tool profile*). A chat agent can
hand heavy work to a coder agent.

### Scheduled tasks & reminders

Just ask in chat — "remind me to stretch at 3pm", or "every weekday at 9am, draft
my standup". The agent schedules it and claw fires it on time (one-shot or
recurring). To see and manage everything, open **admin → tasks**: it lists every
scheduled task across all agents with its next fire time, and **pause / resume /
cancel** buttons.

(Schedules use `CLAW_TIMEZONE` for the cron grid — set it to your zone.)

### Questions & approvals

Two kinds of interactive cards can appear in a chat:

- **Question card** — when an agent needs a decision, it shows a multiple-choice
  card; click an option and the agent continues. Unanswered questions expire after
  a day.
- **Approval card** — when an agent tries something sensitive (deleting an
  endpoint, creating another agent), it's **held** and you get an **Allow / Deny**
  card. Nothing happens until you choose. You, the owner, are never gated — only
  agents are.

## The admin panel

Click **⚙ admin** (sidebar) for a self-rendering control panel. Each section is an
**editable list** — every item is a row you can change inline (with **save** and
**delete**), plus an **add** row to create new ones:

- **endpoints** — your model endpoints (add / edit / delete).
- **groups** — your agents and their provider / endpoint / model / tool-profile /
  CLI-scope; also `groups-create` for new agents.
- **roles** — grant `owner` / `admin` to users.
- **wirings** — which agents are attached to which chats.
- **tasks** — the cross-agent schedule view with pause/resume/cancel.

Anything you can do here, you can also do from the [command line](#command-line-admin).

## Configuration reference

Set these in `.env` (Compose reads it automatically).

| Variable | Default | What it does |
|---|---|---|
| `CLAW_AUTH_TOKEN` | *(generated)* | Your login token. If unset, one is generated, saved to `/data/auth_token`, and printed to the logs. |
| `CLAW_PORT` | `8080` | Port the web UI listens on. |
| `CLAW_TIMEZONE` | `UTC` | Your IANA timezone (e.g. `Europe/Paris`) for schedules. |
| `CLAW_DATA_DIR` | `/data` | Where everything is stored (keep this on a volume). |
| `CLAW_DEFAULT_ENDPOINT` | — | Optional fallback endpoint name for agents without one. |
| `CLAW_DEFAULT_MODEL` | — | Optional fallback model for agents without one. |

## Your data & backups

Everything claw knows lives under the `/data` volume:

```
/data/
  central.db              # your agents, chats, message history, schedules
  auth_token              # the generated login token (if not set by env)
  sessions/…              # each conversation's working database
  groups/<agent>/         # each agent's files (incl. its AGENT.md persona)
  logs/
```

**Back up** by snapshotting the volume while claw is stopped (so the databases are
at rest):

```bash
docker compose stop claw
docker run --rm -v rust-claw_claw-data:/data -v "$PWD":/backup debian:bookworm-slim \
  tar czf /backup/claw-backup.tgz -C /data .
docker compose start claw
```

Restore by extracting that tarball back into the volume.

> **Tip:** each agent's personality lives in `groups/<agent>/AGENT.md` — a plain
> Markdown file claw scaffolds on first run. Edit it to change how an agent
> behaves; your edits are never overwritten.

## Upgrading

claw is stateless outside `/data`, so upgrading is just a new image on the same
volume:

```bash
git pull
docker compose up --build -d
```

Database schema changes are applied automatically on start, so old data keeps
working.

## Security notes

- claw is **single-user / single-token** by design — anyone with the token has
  full access. Put it behind your own TLS/reverse proxy or VPN if you expose it
  beyond localhost, and use a strong `CLAW_AUTH_TOKEN`.
- The login cookie is HttpOnly; every page and API route except health and the
  login page requires it.
- Agents are sandboxed by *policy*, not by a kernel boundary: tool profiles and
  approval gates limit what an agent will do, and an agent only reaches the admin
  surface if you give its group a non-`disabled` CLI scope. A coder agent's shell
  runs **inside the container** — the container is the isolation boundary, so
  don't hand untrusted models a coder profile on a host you care about.

## Troubleshooting

- **The assistant just repeats what I say.** No model is configured yet — see
  [Make it smart](#make-it-smart-connect-a-model). (The default provider is
  `echo`.)
- **I lost the login token.** `docker compose logs claw | grep token`, or set
  `CLAW_AUTH_TOKEN` in `.env` and restart.
- **Replies error out.** Check the endpoint base URL, model name, and key in
  **admin → endpoints / groups**, then `docker compose logs -f claw`.
- **A request needs my approval and I missed it.** It's an Allow/Deny card in the
  chat where it was requested; scroll up to it.
- **Port already in use.** Change `CLAW_PORT` in `.env`.

## Command-line admin

The same admin commands are available inside the container over a local socket —
handy for scripting:

```bash
docker compose exec claw claw endpoints list
docker compose exec claw claw groups update --id <group-id> --model <model>
docker compose exec claw claw groups create --name "Researcher"
```

Run `docker compose exec claw claw <resource> <verb>` for any resource shown in
the admin panel (endpoints, groups, roles, wirings).

## For developers

The whole thing is one Rust crate, no Node, no build step for the UI (Askama
templates + hand-written CSS/JS embedded in the binary).

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
bash scripts/smoke.sh   # build + boot the container, check health + auth
```

- Architecture & rationale: [ARCHITECTURE.md](ARCHITECTURE.md)
- Roadmap & build log: [PLAN.md](PLAN.md)
- Coding rules: [CLAUDE.md](CLAUDE.md)
- Why no `pi`/TypeScript: [docs/decisions/001-drop-pi.md](docs/decisions/001-drop-pi.md)
- UI reference shots: [screenshots/](screenshots/)
