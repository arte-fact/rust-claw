# 001 — Drop pi; build the coding agent natively in Rust

**Date:** 2026-06-11 · **Status:** accepted · **Supersedes:** ARCHITECTURE §8.4/§8.6 (pi
provider + TypeScript tool extension) as originally written.

## Context

The original design used [pi](https://github.com/badlogic/pi-mono) (TypeScript, Bun/Node) as the
coding-agent provider, driven over its RPC mode, with a TypeScript extension writing tool results
into the session DB. M4.1–M4.3 built and tested the RPC client, the prompt formatter, and the
extension. M4.4 would have hardened the dependency into the architecture: Node + pnpm + pi in the
runtime image, `models.json` materialization, and permanent protocol-drift tracking.

## What pi actually provides vs what claw needs

Pi's own philosophy is minimal: **seven tools** (`bash`, `read`, `write`, `edit`, `grep`, `find`,
`ls`), a plain agent loop, JSONL session persistence — and explicitly **no MCP**. Everything else
(TUI, themes, skills, compaction, session branching) is interactive-use comfort never exercised
over RPC.

By the end of M3, claw already had: the agent loop with multi-round tool dispatch, an
OpenAI-compatible client (OpenRouter + every local inference server), session memory (the session
DB transcript), tool-call error feedback, and context windowing. **The remaining delta was a bash
tool and three file tools** — `grep`/`find`/`ls` are bash one-liners.

## Decision

1. Remove pi entirely: `src/providers/pi/`, `pi-extension/`, the Node CI job, the `Pi` provider
   enum variant, the `pi/` session subdir. The repo is 100 % Rust; the runtime image needs no Node.
2. Extend the **native provider** into the coding agent: `bash` + `read`/`write`/`edit` tools,
   gated by a per-group **tool profile** (`chat` = messaging tools only; `coder` = + bash/files).
3. Keep the `AgentProvider` trait seam — pi (or any external CLI agent) can return later as an
   optional provider. The M4.1–M4.3 code lives in git history (`01f65dc` and earlier).
4. The XML batch formatter (M4.2) is retained: it is the canonical prompt renderer for any future
   prompt-consuming provider and is provider-agnostic.

## Costs accepted

- No pi `edit` polish, LSP, or skills; our `edit` is exact-string-replace.
- No Claude Pro/Max subscription auth (pi perk); the native client is API-key/OpenAI-compatible,
  which covers the planned OpenRouter + local-inference setups.
- Cross-turn tool memory: the transcript persists chat in/out only; intermediate tool rounds are
  not yet replayed into later context (tracked in PLAN 4.7).

## MCP

Considered and deferred, in two forms: an **MCP client** in the native loop (the future
plugin/tool-extensibility seam, replacing the old `add_mcp_server` idea) and **claw as an MCP
server** (an additional control surface for external agents — attractive later, but as a
*replacement* it would forfeit the always-on daemon, scheduling, and the web UI). Both are
backlog items in PLAN M9+.
