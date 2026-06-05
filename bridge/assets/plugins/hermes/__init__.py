"""Agent Buddy -- Hermes plugin.

Loaded by Hermes (the installer copied this package into
``$HERMES_HOME/plugins/agent-buddy/`` and enabled it). Hermes calls
``register(ctx)`` at load; we subscribe to lifecycle hooks via
``ctx.register_hook(name, cb)`` and translate each into Agent Buddy's canonical
event names, fire-and-forgetting them to the local Agent Buddy daemon's loopback
HTTP listener (which drives the hardware buddy's animation).

Invariants shared with the other Agent Buddy plugins: standard-library only,
never block Hermes (each POST runs on a short-lived daemon thread), and discover
the live ``{http_port, token}`` from the daemon's ``endpoint.json`` named by a
sibling ``bridge.json`` the installer wrote.
"""

from __future__ import annotations

import json
import os
import sys
import threading
import urllib.request
from typing import Any, Callable, Dict, Optional, Tuple

_PLUGIN_DIR = os.path.dirname(os.path.abspath(__file__))
_BRIDGE_PATH = os.path.join(_PLUGIN_DIR, "bridge.json")
_AGENT_ID = "hermes"
_POST_TIMEOUT = 0.6

# Hermes hook name -> canonical Agent Buddy event. ``post_llm_call`` is
# deliberately omitted: it fires after every model call (including mid-turn ones
# before a tool call), so mapping it to a turn-end Stop would flicker. The real
# turn boundary is ``on_session_end`` (fired after each conversation turn);
# ``on_session_finalize`` is the process-exit session boundary.
_HOOK_TO_EVENT: Dict[str, str] = {
    "on_session_start": "SessionStart",
    "on_session_reset": "SessionStart",
    "pre_llm_call": "UserPromptSubmit",
    "pre_tool_call": "PreToolUse",
    "post_tool_call": "PostToolUse",
    "on_session_end": "Stop",
    "on_session_finalize": "SessionEnd",
}

# The hooks we register (must match plugin.yaml's `hooks:` list).
HOOKS: Tuple[str, ...] = tuple(_HOOK_TO_EVENT.keys())


def _endpoint() -> Optional[Dict[str, Any]]:
    """Read the daemon endpoint named by our sibling bridge.json. None if the
    daemon isn't running / nothing is published yet."""
    try:
        with open(_BRIDGE_PATH, "r", encoding="utf-8") as fh:
            bridge = json.load(fh)
        with open(bridge["endpoint"], "r", encoding="utf-8") as fh:
            ep = json.load(fh)
        if not ep.get("http_port"):
            return None
        return {
            "port": int(ep["http_port"]),
            "token": ep.get("token", ""),
            "agent": bridge.get("agent", _AGENT_ID),
        }
    except Exception:
        return None


def _post(event: str, fields: Dict[str, Any]) -> None:
    """Fire-and-forget one canonical event on a short-lived daemon thread."""
    ep = _endpoint()
    if not ep:
        return
    body = {"token": ep["token"], "agent": ep["agent"], "event": event}
    body.update({k: v for k, v in fields.items() if v is not None})
    data = json.dumps(body).encode("utf-8")
    url = "http://127.0.0.1:%d/" % ep["port"]

    def _send() -> None:
        try:
            req = urllib.request.Request(
                url, data=data, headers={"Content-Type": "application/json"}, method="POST"
            )
            urllib.request.urlopen(req, timeout=_POST_TIMEOUT).close()
        except Exception:
            pass

    threading.Thread(target=_send, daemon=True).start()


def _session_id(kwargs: Dict[str, Any]) -> str:
    for key in ("session_id", "session_key", "sessionId"):
        val = kwargs.get(key)
        if isinstance(val, str) and val:
            return val
    return ""


def _tool_result_has_error(result: Any) -> bool:
    """True if a tool result signals an error: a dict with a truthy ``error``
    or a non-zero int ``exit_code``; a JSON-string result is parsed the same way
    (falling back to an ``"error"`` substring scan). Mirrors the reference."""
    if isinstance(result, dict):
        if result.get("error"):
            return True
        code = result.get("exit_code")
        return isinstance(code, int) and code != 0
    if isinstance(result, str):
        text = result.strip()
        if not text:
            return False
        try:
            parsed = json.loads(text)
            if isinstance(parsed, dict):
                if parsed.get("error"):
                    return True
                code = parsed.get("exit_code")
                return isinstance(code, int) and code != 0
        except Exception:
            pass
        return '"error"' in text[:500].lower()
    return False


def _first_string(*vals: Any) -> str:
    for v in vals:
        if isinstance(v, str) and v.strip():
            return v.strip()
    return ""


def _thread_env_cwd() -> str:
    """Hermes-WebUI keeps the per-run cwd in a thread-local env; best-effort."""
    try:
        config = sys.modules.get("api.config")
        env = getattr(getattr(config, "_thread_ctx", None), "env", None) if config else None
        if isinstance(env, dict):
            return _first_string(env.get("TERMINAL_CWD"))
    except Exception:
        pass
    return ""


def _runtime_cwd() -> str:
    """Resolve the agent's working dir from the environment (Hermes passes no
    ``cwd`` kwarg): thread env TERMINAL_CWD -> env TERMINAL_CWD -> PWD -> getcwd."""
    return _first_string(
        _thread_env_cwd(),
        os.environ.get("TERMINAL_CWD"),
        os.environ.get("PWD"),
        os.getcwd(),
    )


def _make_callback(hook: str) -> Callable[..., Optional[Dict[str, Any]]]:
    base_event = _HOOK_TO_EVENT[hook]

    def _cb(**kwargs: Any) -> Optional[Dict[str, Any]]:
        try:
            # Escalate to the failure events when the payload signals one (the
            # profile event_map maps both to the error state).
            event = base_event
            if hook == "post_tool_call" and _tool_result_has_error(kwargs.get("result")):
                event = "PostToolUseFailure"
            elif (
                hook == "on_session_end"
                and kwargs.get("completed") is False
                and kwargs.get("interrupted") is not True
            ):
                event = "StopFailure"
            _post(
                event,
                {
                    "session_id": _session_id(kwargs),
                    "cwd": kwargs.get("cwd") or _runtime_cwd(),
                    "tool_name": kwargs.get("tool_name"),
                },
            )
        except Exception:
            pass
        # pre_tool_call may return a block directive; we never gate, so allow.
        return None

    return _cb


def register(ctx: Any) -> None:
    """Hermes plugin entry point: subscribe our hooks."""
    register_hook = getattr(ctx, "register_hook", None)
    if not callable(register_hook):
        return
    for hook in HOOKS:
        try:
            register_hook(hook, _make_callback(hook))
        except Exception:
            pass
