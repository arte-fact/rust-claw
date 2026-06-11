-- Per-session SQLite schema, include_str!'d by src/session/mod.rs.
-- Seq parity contract: the host router writes messages_in (EVEN seq);
-- agent runs write messages_out (ODD seq) — see Seq in src/protocol/message.rs.

CREATE TABLE IF NOT EXISTS messages_in (
  id                TEXT PRIMARY KEY,
  seq               INTEGER UNIQUE,
  kind              TEXT NOT NULL,
  timestamp         TEXT NOT NULL,
  status            TEXT NOT NULL DEFAULT 'pending',
  process_after     TEXT,
  recurrence        TEXT,
  series_id         TEXT,
  tries             INTEGER NOT NULL DEFAULT 0,
  trigger           INTEGER NOT NULL DEFAULT 1,
  platform_id       TEXT,
  channel_type      TEXT,
  thread_id         TEXT,
  content           TEXT NOT NULL,
  source_session_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_in_series ON messages_in(series_id);
CREATE INDEX IF NOT EXISTS idx_messages_in_status ON messages_in(status);

CREATE TABLE IF NOT EXISTS messages_out (
  id            TEXT PRIMARY KEY,
  seq           INTEGER UNIQUE,
  in_reply_to   TEXT,
  timestamp     TEXT NOT NULL,
  deliver_after TEXT,
  recurrence    TEXT,
  kind          TEXT NOT NULL,
  platform_id   TEXT,
  channel_type  TEXT,
  thread_id     TEXT,
  content       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS delivered (
  message_out_id      TEXT PRIMARY KEY,
  platform_message_id TEXT,
  status              TEXT NOT NULL DEFAULT 'delivered',
  delivered_at        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS destinations (
  name           TEXT PRIMARY KEY,
  display_name   TEXT,
  type           TEXT NOT NULL,
  channel_type   TEXT,
  platform_id    TEXT,
  agent_group_id TEXT
);

CREATE TABLE IF NOT EXISTS session_routing (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  channel_type TEXT,
  platform_id  TEXT,
  thread_id    TEXT
);
