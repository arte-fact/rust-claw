// Session-DB writer for the pi agent. The host (claw, Rust) owns messages_in
// with EVEN seq; this writes messages_out with ODD seq. The seq logic mirrors
// `Seq::next_agent_after` in src/protocol/message.rs — the schema is shared via
// src/session/schema.sql (§14 contract).

import { copyFileSync, mkdirSync } from "node:fs";
import { basename, join } from "node:path";
import { randomUUID } from "node:crypto";
import { DatabaseSync } from "node:sqlite";

export interface Session {
  db: DatabaseSync;
  dir: string;
}

const INSERT_OUTBOUND = `INSERT INTO messages_out
  (id, seq, in_reply_to, timestamp, deliver_after, recurrence, kind,
   platform_id, channel_type, thread_id, content)
  VALUES (?, ?, ?, ?, NULL, NULL, ?, NULL, NULL, NULL, ?)`;

export function openSession(sessionDir: string): Session {
  const db = new DatabaseSync(join(sessionDir, "session.db"));
  db.exec("PRAGMA busy_timeout = 5000");
  return { db, dir: sessionDir };
}

/// Next ODD seq, one past MAX(seq) across both tables. Even floor -> floor+1
/// (already odd); odd floor -> floor+2. Empty tables -> 1.
export function nextAgentSeq(db: DatabaseSync): number {
  const row = db
    .prepare(
      `SELECT MAX(s) AS m FROM (
         SELECT MAX(seq) AS s FROM messages_in
         UNION ALL
         SELECT MAX(seq) AS s FROM messages_out
       )`,
    )
    .get() as { m: number | null } | undefined;
  const floor = row?.m ?? -1;
  const candidate = floor + 1;
  return candidate % 2 === 0 ? candidate + 1 : candidate;
}

function insertOutbound(
  db: DatabaseSync,
  id: string,
  kind: string,
  content: unknown,
  inReplyTo: string | null,
): void {
  const seq = nextAgentSeq(db);
  db.prepare(INSERT_OUTBOUND).run(
    id,
    seq,
    inReplyTo,
    new Date().toISOString(),
    kind,
    JSON.stringify(content),
  );
}

/// Reply to the user — a chat row whose routing the host fills from session_routing.
export function sendMessage(session: Session, text: string, inReplyTo: string | null = null): string {
  const id = `out-${randomUUID()}`;
  insertOutbound(session.db, id, "chat", { text }, inReplyTo);
  return id;
}

/// Stage a file under outbox/<message_id>/ and reference it by name; the host
/// reads the staged file and uploads it on delivery.
export function sendFile(
  session: Session,
  sourcePath: string,
  text: string | null = null,
  inReplyTo: string | null = null,
): string {
  const id = `out-${randomUUID()}`;
  const name = basename(sourcePath);
  const outbox = join(session.dir, "outbox", id);
  mkdirSync(outbox, { recursive: true });
  copyFileSync(sourcePath, join(outbox, name));
  const content: { files: string[]; text?: string } = { files: [name] };
  if (text !== null) {
    content.text = text;
  }
  insertOutbound(session.db, id, "chat", content, inReplyTo);
  return id;
}
