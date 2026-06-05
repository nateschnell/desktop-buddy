// Agent Buddy — opencode plugin.
//
// Runs inside the opencode process (Bun runtime). It translates opencode's
// session/tool events into Agent Buddy's canonical event names and fire-and-
// forgets them to the local Agent Buddy daemon's loopback HTTP listener, which
// drives the hardware buddy's animation/state.
//
// Design invariants (match the other Agent Buddy plugins):
//   - zero third-party deps — Bun/Node built-ins only;
//   - fire-and-forget: an event handler NEVER awaits the POST, so a slow or
//     absent daemon can never stall opencode;
//   - discovery is indirect: a sibling `bridge.json` (written by the installer)
//     names the daemon's `endpoint.json`, which carries the live { http_port,
//     token }. Reading it per-event means a daemon restart (new port/token) is
//     picked up with no plugin change.
//
// opencode loads this because the installer added this directory's absolute path
// to `~/.config/opencode/opencode.json`'s `plugin` array. opencode's plugin
// contract: a default-exported async function returning `{ event }`.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const AGENT_ID = "opencode";
const PLUGIN_DIR = dirname(fileURLToPath(import.meta.url));
const BRIDGE_PATH = join(PLUGIN_DIR, "bridge.json");

// Child sessions (opencode's task tool spawns them) end with `session.idle`
// like the root, but should map to SessionEnd rather than a turn Stop.
const childSessions = new Set();
// De-dupe consecutive identical (session,event) posts — opencode emits
// `message.part.updated` rapidly while a tool streams output.
let lastKey = "";

/** Read the daemon endpoint named by our sibling bridge.json. Null if anything
 *  is missing — the daemon may simply not be running. */
function endpoint() {
  try {
    const bridge = JSON.parse(readFileSync(BRIDGE_PATH, "utf8"));
    const ep = JSON.parse(readFileSync(bridge.endpoint, "utf8"));
    if (!ep || !ep.http_port) return null;
    return { port: ep.http_port, token: ep.token || "", agent: bridge.agent || AGENT_ID };
  } catch {
    return null;
  }
}

/** Fire-and-forget POST of one canonical event. Never throws, never awaited. */
function post(event, extra) {
  const ep = endpoint();
  if (!ep) return;
  const body = JSON.stringify({ token: ep.token, agent: ep.agent, event, ...extra });
  // Bun/Node global fetch; swallow every failure (daemon down, etc.).
  fetch(`http://127.0.0.1:${ep.port}/`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body,
    // A short signal so a wedged socket can't pin a handler.
    signal: AbortSignal.timeout(400),
  }).catch(() => {});
}

/** Translate one opencode event into a canonical Agent Buddy event (or null). */
function classify(event) {
  const t = event?.type;
  const p = event?.properties || {};
  switch (t) {
    case "session.created": {
      const parent = p.info && p.info.parentID;
      const sid = p.info && p.info.id;
      if (parent && sid) childSessions.add(sid);
      return { event: "SessionStart", session_id: sid };
    }
    case "session.status":
      // opencode flips a session "busy" when the model starts working.
      if (p.status && p.status.type === "busy")
        return { event: "UserPromptSubmit", session_id: p.sessionID };
      return null;
    case "message.part.updated": {
      const part = p.part || {};
      if (part.type === "tool") {
        const status = (part.state && part.state.status) || "";
        if (status === "running")
          return { event: "PreToolUse", session_id: p.sessionID, tool_name: part.tool };
        if (status === "completed")
          return { event: "PostToolUse", session_id: p.sessionID, tool_name: part.tool };
        if (status === "error")
          return { event: "PostToolUseFailure", session_id: p.sessionID, tool_name: part.tool };
        return null;
      }
      if (part.type === "compaction")
        return { event: "PreCompact", session_id: p.sessionID };
      return null;
    }
    case "session.idle": {
      const sid = p.sessionID;
      if (sid && childSessions.has(sid)) {
        childSessions.delete(sid);
        return { event: "SessionEnd", session_id: sid };
      }
      return { event: "Stop", session_id: sid };
    }
    case "session.error":
      return { event: "StopFailure", session_id: p.sessionID };
    case "session.deleted":
    case "server.instance.disposed":
      return { event: "SessionEnd", session_id: p.sessionID };
    default:
      return null;
  }
}

const plugin = async (ctx) => {
  const cwd = (ctx && ctx.directory) || "";
  return {
    event: async ({ event }) => {
      const c = classify(event);
      if (!c) return;
      const key = `${c.session_id || ""}:${c.event}`;
      if (key === lastKey) return; // skip a repeated identical state
      lastKey = key;
      const { event: name, ...extra } = c;
      post(name, { cwd, ...extra });
    },
  };
};

export default plugin;
