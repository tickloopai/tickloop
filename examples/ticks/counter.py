#!/usr/bin/env python3
"""A nonfinancial Tick using only Python's standard library and stdin/stdout."""

import datetime
import hashlib
import json
import os
import sys
import threading


INSTANCE = os.environ["TICKLOOP_INSTANCE_ID"]
MAX_REQUEST_BYTES = 512 * 1024
HISTORY_LIMIT = 256
STATE_LOCK = threading.RLock()
OUTPUT_LOCK = threading.Lock()
value = 0
sequence = 0
history = []


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def send(frame):
    with OUTPUT_LOCK:
        print(json.dumps({"jsonrpc": "2.0", **frame}), flush=True)


def draw():
    if sys.stderr.isatty():
        print(f"\r\033[2KCounter: {value}  |  type + then Enter to change; Ctrl+C exits", file=sys.stderr, flush=True)


def increment(amount):
    global value, sequence
    with STATE_LOCK:
        value += amount
        sequence += 1
        update = {
            "version": 3,
            "tick_instance_id": INSTANCE,
            "source": "example.counter",
            "sequence": sequence,
            "observed_at": now(),
            "data": {"value": value},
        }
        history.append({"sequence": sequence, "value": value})
        del history[:-HISTORY_LIMIT]
        send({"method": "tick/update", "params": update})
        draw()
        return {"value": value}


def terminal_input():
    try:
        with os.fdopen(os.dup(sys.stderr.fileno()), "r", closefd=True) as terminal:
            for line in terminal:
                if line.strip() == "+":
                    increment(1)
    except OSError as error:
        print(f"Counter keyboard unavailable: {error}", file=sys.stderr)


def respond(request):
    method = request.get("method")
    if method == "tick/describe":
        return {
            "protocol": 2,
            "tick_type": "example.counter",
            "title": "Counter",
            "description": "An external nonfinancial Tick. Keyboard and tool calls share one value.",
            "recover_with": "history",
            "tools": [
                {
                    "name": "increment",
                    "description": "Add an integer to the visible counter.",
                    "params": {"type": "object", "properties": {"amount": {"type": "integer", "minimum": -100, "maximum": 100}}, "required": ["amount"], "additionalProperties": False},
                    "mutates": True,
                },
                {
                    "name": "history",
                    "description": "Read the last 256 captured counter changes; earlier history is unavailable.",
                    "params": {"type": "object", "additionalProperties": False},
                    "mutates": False,
                },
            ],
        }
    if method == "tick/read":
        with STATE_LOCK:
            return {"version": 3, "tick_instance_id": INSTANCE, "initialized": True, "sequence": sequence, "observed_at": now(), "data": {"value": value}}
    if method == "tool/call":
        params = request.get("params", {})
        if not isinstance(params, dict) or not isinstance(params.get("args", {}), dict):
            raise ValueError("tool parameters and arguments must be objects")
        expected_digest = params.get("expected_descriptor_digest")
        descriptor = respond({"method": "tick/describe"})
        digest = hashlib.sha256(json.dumps(descriptor, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()
        if expected_digest is not None and expected_digest != digest:
            raise ValueError("Tick tool catalog changed; prepare and approve again")
        if params.get("tool") == "increment":
            arguments = params.get("args", {})
            amount = arguments.get("amount")
            if set(arguments) != {"amount"} or type(amount) is not int or not -100 <= amount <= 100:
                raise ValueError("amount must be an integer between -100 and 100")
            return increment(amount)
        if params.get("tool") == "history":
            if params.get("args", {}):
                raise ValueError("history accepts no arguments")
            with STATE_LOCK:
                return {"changes": list(history), "earlier_history_unavailable": sequence > HISTORY_LIMIT}
        raise ValueError("unknown Tick Tool")
    raise ValueError("unknown method")


def main():
    draw()
    if sys.stderr.isatty():
        threading.Thread(target=terminal_input, daemon=True).start()
    while True:
        line = sys.stdin.buffer.readline(MAX_REQUEST_BYTES + 1)
        if not line:
            return
        if len(line) > MAX_REQUEST_BYTES:
            print("Tick request exceeds 512 KiB", file=sys.stderr)
            return
        request = {}
        try:
            request = json.loads(line)
            if not isinstance(request, dict) or request.get("jsonrpc") != "2.0" or "id" not in request:
                raise ValueError("expected a JSON-RPC 2.0 request with an id")
            result = respond(request)
            send({"id": request["id"], "result": result})
        except (ValueError, TypeError) as error:
            identity = request.get("id") if isinstance(request, dict) else None
            send({"id": identity, "error": {"code": -32602, "message": str(error)}})


if __name__ == "__main__":
    main()
