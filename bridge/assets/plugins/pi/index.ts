// Agent Buddy -- pi extension.
//
// Loaded by pi from ~/.pi/agent/extensions/agent-buddy/index.ts (the installer
// copied it there). pi's extension contract: a default-exported function that
// receives the ExtensionAPI and subscribes via `pi.on(eventName, cb)`. We
// translate pi's native events into Agent Buddy's canonical event names and
// fire-and-forget them to the local Agent Buddy daemon's loopback HTTP listener.
//
// Same invariants as the sibling plugins: Node built-ins only, never block pi,
// and discover the live { http_port, token } from the daemon's endpoint.json
// named by a sibling bridge.json the installer wrote. The `import type` below is
// erased at compile time, so the only runtime imports are Node built-ins.

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import * as http from "node:http";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const AGENT_ID = "agent-buddy";
const POST_TIMEOUT_MS = 600;
// Deterministic install location (pi has no extensions-dir env override).
const BRIDGE_PATH = path.join(os.homedir(), ".pi", "agent", "extensions", "agent-buddy", "bridge.json");

// pi native event -> canonical Agent Buddy event. `agent_end` is the turn
// boundary; `session_shutdown` is the process/session boundary.
const EVENT_MAP: Record<string, string> = {
  session_start: "SessionStart",
  before_agent_start: "UserPromptSubmit",
  tool_call: "PreToolUse",
  // tool_result is split below on isError.
  agent_end: "Stop",
  session_before_compact: "PreCompact",
  session_compact: "PostCompact",
  session_shutdown: "SessionEnd",
};

function endpoint(): { port: number; token: string; agent: string } | null {
  try {
    const bridge = JSON.parse(fs.readFileSync(BRIDGE_PATH, "utf8"));
    const ep = JSON.parse(fs.readFileSync(bridge.endpoint, "utf8"));
    if (!ep || !ep.http_port) return null;
    return { port: ep.http_port, token: ep.token || "", agent: bridge.agent || AGENT_ID };
  } catch {
    return null;
  }
}

function post(event: string, extra: Record<string, unknown>): void {
  const ep = endpoint();
  if (!ep) return;
  const payload = JSON.stringify({ token: ep.token, agent: ep.agent, event, ...extra });
  try {
    const req = http.request(
      {
        hostname: "127.0.0.1",
        port: ep.port,
        path: "/",
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Content-Length": Buffer.byteLength(payload),
        },
        timeout: POST_TIMEOUT_MS,
      },
      (res) => res.resume(),
    );
    req.on("timeout", () => req.destroy());
    req.on("error", () => {});
    req.write(payload);
    req.end();
  } catch {
    /* fire-and-forget */
  }
}

function sessionId(ctx: ExtensionContext): string {
  try {
    const sm = (ctx as any)?.sessionManager;
    if (sm && typeof sm.getSessionId === "function") return sm.getSessionId() || "";
  } catch {
    /* ignore */
  }
  return "";
}

export default function agentBuddyExtension(pi: ExtensionAPI): void {
  const handle = (native: string) => (event: any, ctx: ExtensionContext) => {
    try {
      // Only report when pi has an interactive UI (matches upstream behavior;
      // headless/scripted runs shouldn't drive the desk buddy). When hasUI is
      // unset, fall back to a TTY check so a scripted run is still suppressed.
      const hasUI = ctx && (ctx as any).hasUI;
      if (hasUI === false) return;
      if (hasUI === undefined && !(process.stdin.isTTY && process.stdout.isTTY)) return;
      const cwd = (ctx && (ctx as any).cwd) || "";
      const sid = sessionId(ctx);
      let name = EVENT_MAP[native];
      const extra: Record<string, unknown> = { session_id: sid, cwd };
      if (native === "tool_result") {
        name = event && event.isError ? "PostToolUseFailure" : "PostToolUse";
        extra.tool_name = event && event.toolName;
      } else if (native === "tool_call") {
        extra.tool_name = event && event.toolName;
      }
      if (!name) return;
      post(name, extra);
    } catch {
      /* never let a handler throw into pi */
    }
  };

  for (const native of [
    "session_start",
    "before_agent_start",
    "tool_call",
    "tool_result",
    "agent_end",
    "session_before_compact",
    "session_compact",
    "session_shutdown",
  ]) {
    try {
      pi.on(native as any, handle(native));
    } catch {
      /* ignore an event name this pi version doesn't know */
    }
  }
}
