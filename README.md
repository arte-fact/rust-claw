# claw

A personal AI assistant as **one Rust daemon**. A built-in web UI routes your
messages to an in-process agent loop (any OpenAI-compatible endpoint; chat and
coder tool profiles), with SQLite as the only IO surface. 100 % Rust — no Node,
no second runtime.

- Design: [ARCHITECTURE.md](ARCHITECTURE.md)
- Roadmap / status: [PLAN.md](PLAN.md)
- Why no `pi`/TypeScript: [docs/decisions/001-drop-pi.md](docs/decisions/001-drop-pi.md)

## Quickstart (Docker)

```bash
docker compose up --build -d
docker compose logs claw | grep "generated and saved a login token"
```

Open <http://localhost:8080>, log in with that token, and you have a chat. To get
real answers, point claw at an inference endpoint: open **⚙ admin → endpoints**,
add one (e.g. OpenRouter `https://openrouter.ai/api/v1` with an `api_key`), then
**admin → groups** and set the default group's provider to `native`, its endpoint,
and a model.

Set `CLAW_AUTH_TOKEN` in `compose.yaml` to choose your own token instead of the
generated one (the generated token is saved to `/data/auth_token` and reused
across restarts).

## Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `CLAW_DATA_DIR` | `/data` | Persistent root (DBs, sessions, logs, socket). |
| `CLAW_PORT` | `8080` | Web UI port. |
| `CLAW_AUTH_TOKEN` | *(generated)* | Login token; generated + persisted if unset. |
| `CLAW_TIMEZONE` | `UTC` | IANA tz for schedule next-fire display/evaluation. |
| `CLAW_DEFAULT_ENDPOINT` | — | Fallback endpoint name for groups without one. |
| `CLAW_DEFAULT_MODEL` | — | Fallback model for groups without one. |

## Data & backup

Everything lives under the `/data` volume:

```
/data/
  central.db              # entities, routing, web message ledger (WAL)
  auth_token              # generated login token (0600)
  sessions/<group>/<id>/  # per-session.db + inbox/ + outbox/
  groups/<folder>/        # per-agent-group files: AGENT.md, working files
  logs/
  claw.sock               # admin CLI socket
```

To back up, snapshot the volume while the container is stopped (cleanest, since
SQLite WAL files are consistent at rest):

```bash
docker compose stop claw
docker run --rm -v rust-claw_claw-data:/data -v "$PWD":/backup debian:bookworm-slim \
  tar czf /backup/claw-backup.tgz -C /data .
docker compose start claw
```

Restore by extracting the tarball back into the volume. Upgrades are just a new
image on the same volume — migrations run on boot and are idempotent.

## Admin CLI

The container exposes the same command registry over a unix socket; run admin
commands inside the container:

```bash
docker compose exec claw claw endpoints list
docker compose exec claw claw groups update --id <group-id> --model <model>
```

## Development

Requires Rust 1.96+ (rusqlite's bundled SQLite). The full gate:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Container smoke test (build + boot + auth checks): `bash scripts/smoke.sh`.
UI changes must refresh `screenshots/` — see [CLAUDE.md](CLAUDE.md).
