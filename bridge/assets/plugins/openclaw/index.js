// Agent Buddy — OpenClaw plugin.
//
// Loaded by OpenClaw on startup (the installer added this directory to
// `~/.openclaw/openclaw.json`'s `plugins.load.paths` and enabled the entry, and
// the manifest sets `activation.onStartup`). OpenClaw's plugin contract: a
// default export `{ id, name, register(api) }`; `api.on(hook, cb, opts)`
// subscribes to lifecycle hooks.
//
// We translate OpenClaw's hooks into Agent Buddy's canonical event names and
// fire-and-forget them to the local daemon's loopback HTTP listener. Same
// invariants as the other plugins: zero third-party deps (Node built-ins),
// never block OpenClaw, discover the live { http_port, token } from the
// daemon's endpoint.json (named by a sibling bridge.json the installer wrote).

import { readFileSync } from "node:fs";
import { request } from "node:http";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const AGENT_ID = "openclaw";
const PLUGIN_DIR = dirname(fileURLToPath(import.meta.url));
const BRIDGE_PATH = join(PLUGIN_DIR, "bridge.json");
const POST_TIMEOUT_MS = 600;
// `model_call_ended` fires after every model call, including mid-turn ones that
// precede a tool call. Debounce a turn-end Stop so a tool call arriving shortly
// after cancels it — matching how a real turn ends only when the model stops
// *and* nothing follows.
const STOP_DEBOUNCE_MS = 1200;

const HOOK_NAMES = [
  "session_start",
  "model_call_started",
  "model_call_ended",
  "before_tool_call",
  "after_tool_call",
  "before_compaction",
  "after_compaction",
  "session_end",
];

const pendingStops = new Map(); // sessionId -> timer

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

function post(event, extra) {
  const ep = endpoint();
  if (!ep) return;
  const payload = JSON.stringify({ token: ep.token, agent: ep.agent, event, ...extra });
  try {
    const req = request(
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
  } catch {}
}

function clearPendingStop(sid) {
  const t = pendingStops.get(sid);
  if (t) {
    clearTimeout(t);
    pendingStops.delete(sid);
  }
}

function scheduleStop(sid, cwd, failed) {
  clearPendingStop(sid);
  const t = setTimeout(() => {
    pendingStops.delete(sid);
    post(failed ? "StopFailure" : "Stop", { session_id: sid, cwd });
  }, STOP_DEBOUNCE_MS);
  // Don't keep the process alive just for a pending buddy ping.
  if (t.unref) t.unref();
  pendingStops.set(sid, t);
}

function handleHook(hook, event, ctx) {
  const sid = (event && event.sessionId) || (ctx && ctx.sessionId)
    || (event && event.sessionKey) || (ctx && ctx.sessionKey)
    || (event && event.runId) || (ctx && ctx.runId) || "openclaw:default";
  const cwd = (ctx && ctx.workspaceDir) || "";
  switch (hook) {
    case "session_start":
      post("SessionStart", { session_id: sid, cwd });
      break;
    case "model_call_started":
      clearPendingStop(sid); // a new model call → still working, cancel turn-end
      post("UserPromptSubmit", { session_id: sid, cwd });
      break;
    case "before_tool_call":
      clearPendingStop(sid);
      post("PreToolUse", { session_id: sid, cwd, tool_name: (ctx && ctx.toolName) || (event && event.toolName) });
      break;
    case "after_tool_call": {
      const err = event && event.error;
      post(err ? "PostToolUseFailure" : "PostToolUse", {
        session_id: sid,
        cwd,
        tool_name: (ctx && ctx.toolName) || (event && event.toolName),
      });
      break;
    }
    case "before_compaction":
      post("PreCompact", { session_id: sid, cwd });
      break;
    case "after_compaction":
      post("PostCompact", { session_id: sid, cwd });
      break;
    case "model_call_ended": {
      const outcome = event && event.outcome;
      const failureKind = (event && typeof event.failureKind === "string") ? event.failureKind : "";
      // A user-aborted/terminated turn is a normal end, not a failure.
      const failed = outcome === "error" && failureKind !== "aborted" && failureKind !== "terminated";
      scheduleStop(sid, cwd, failed);
      break;
    }
    case "session_end": {
      const reason = (event && event.reason) || "";
      // "new"/"reset"/"compaction" are internal churn, not a real session end.
      if (reason !== "new" && reason !== "reset" && reason !== "compaction") {
        clearPendingStop(sid);
        post("SessionEnd", { session_id: sid, cwd });
      }
      break;
    }
    default:
      break;
  }
}

export default {
  id: "agent-buddy",
  name: "Agent Buddy",
  register(api) {
    if (!api || typeof api.on !== "function") return;
    for (const hook of HOOK_NAMES) {
      api.on(
        hook,
        (event, ctx) => {
          try {
            handleHook(hook, event, ctx);
          } catch {}
        },
        { priority: -100, timeoutMs: 1000 },
      );
    }
  },
};
