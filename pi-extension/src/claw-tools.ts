// pi extension: registers claw's messaging tools. Loaded by pi at runtime from
// the agent's extensions directory; the session DB path arrives in CLAW_SESSION_DIR
// (set by the host when it spawns pi). The actual DB writes live in session-db.ts,
// which is unit-tested without pi (§14).

import { Type } from "@sinclair/typebox";
import { openSession, sendFile, sendMessage, type Session } from "./session-db.ts";

function withSession<T>(run: (session: Session) => T): T {
  const dir = process.env.CLAW_SESSION_DIR;
  if (!dir) {
    throw new Error("CLAW_SESSION_DIR is not set");
  }
  const session = openSession(dir);
  try {
    return run(session);
  } finally {
    session.db.close();
  }
}

const ok = (text: string) => ({ content: [{ type: "text", text }], details: {} });

// deno-lint-ignore no-explicit-any -- pi's extension API object is provided at runtime
export default (pi: any) => {
  pi.registerTool({
    name: "send_message",
    label: "Send message",
    description: "Send a chat message back to the user in this conversation.",
    parameters: Type.Object({
      text: Type.String({ description: "The message text to send." }),
    }),
    execute: async (_toolCallId: string, params: { text: string }) => {
      withSession((session) => sendMessage(session, params.text));
      return ok("sent");
    },
  });

  pi.registerTool({
    name: "send_file",
    label: "Send file",
    description: "Send a file to the user, optionally with an accompanying message.",
    parameters: Type.Object({
      path: Type.String({ description: "Absolute path to the file to send." }),
      text: Type.Optional(Type.String({ description: "Optional message to send with the file." })),
    }),
    execute: async (_toolCallId: string, params: { path: string; text?: string }) => {
      withSession((session) => sendFile(session, params.path, params.text ?? null));
      return ok("sent");
    },
  });
};
