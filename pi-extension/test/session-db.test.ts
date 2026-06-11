// Cross-language contract (§14): this loads the SAME schema the Rust host uses
// (src/session/schema.sql) and verifies the extension's writes — column layout
// and seq parity — match what the Rust reader expects. The seq cases mirror the
// `next_agent_after` table test in src/protocol/message.rs.

import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { nextAgentSeq, openSession, sendFile, sendMessage, type Session } from "../src/session-db.ts";

const SCHEMA = readFileSync(join(import.meta.dirname, "../../src/session/schema.sql"), "utf8");

function freshSession(): Session {
  const dir = mkdtempSync(join(tmpdir(), "claw-sess-"));
  const session = openSession(dir);
  session.db.exec(SCHEMA);
  return session;
}

function insertHostMessage(session: Session, seq: number): void {
  session.db
    .prepare("INSERT INTO messages_in (id, seq, kind, timestamp, content) VALUES (?, ?, 'chat', 't', '{}')")
    .run(`in-${seq}`, seq);
}

test("send_message writes a chat row with the first odd seq", () => {
  const session = freshSession();
  const id = sendMessage(session, "hello");

  const row = session.db
    .prepare("SELECT id, seq, kind, content, platform_id FROM messages_out")
    .get() as { id: string; seq: number; kind: string; content: string; platform_id: string | null };
  assert.equal(row.id, id);
  assert.equal(row.seq, 1);
  assert.equal(row.kind, "chat");
  assert.equal(row.platform_id, null, "routing is left to the host's session_routing");
  assert.deepEqual(JSON.parse(row.content), { text: "hello" });
});

test("agent seq stays odd and advances past host even seqs", () => {
  const session = freshSession();
  assert.equal(nextAgentSeq(session.db), 1, "empty -> 1");

  insertHostMessage(session, 0);
  assert.equal(nextAgentSeq(session.db), 1, "floor 0 -> 1");

  sendMessage(session, "a"); // takes seq 1
  assert.equal(nextAgentSeq(session.db), 3, "floor 1 -> 3");

  insertHostMessage(session, 2);
  assert.equal(nextAgentSeq(session.db), 3, "floor 2 -> 3");

  insertHostMessage(session, 8);
  assert.equal(nextAgentSeq(session.db), 9, "floor 8 -> 9");
});

test("two sends interleave odd seqs without colliding", () => {
  const session = freshSession();
  const first = sendMessage(session, "one");
  const second = sendMessage(session, "two");
  assert.notEqual(first, second);

  const seqs = session.db
    .prepare("SELECT seq FROM messages_out ORDER BY seq")
    .all()
    .map((row) => (row as { seq: number }).seq);
  assert.deepEqual(seqs, [1, 3]);
});

test("send_file stages the file under outbox and records it by name", () => {
  const session = freshSession();
  const source = join(session.dir, "chart.png");
  writeFileSync(source, "png-bytes");

  const id = sendFile(session, source, "here is the chart");

  const row = session.db
    .prepare("SELECT content FROM messages_out WHERE id = ?")
    .get(id) as { content: string };
  const content = JSON.parse(row.content) as { files: string[]; text: string };
  assert.deepEqual(content.files, ["chart.png"]);
  assert.equal(content.text, "here is the chart");
  assert.ok(existsSync(join(session.dir, "outbox", id, "chart.png")), "file staged in outbox");
});

test("send_file without a message omits the text field", () => {
  const session = freshSession();
  const source = join(session.dir, "report.pdf");
  writeFileSync(source, "%PDF");

  const id = sendFile(session, source);

  const row = session.db
    .prepare("SELECT content FROM messages_out WHERE id = ?")
    .get(id) as { content: string };
  const content = JSON.parse(row.content) as Record<string, unknown>;
  assert.deepEqual(content, { files: ["report.pdf"] });
});
