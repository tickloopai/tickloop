#!/usr/bin/env python3
"""A nonfinancial Tick speaking Tick protocol 3 with only Python's standard library."""

import json
import os
import sys
import threading


MAX_REQUEST_BYTES = 512 * 1024
HISTORY_LIMIT = 256
# The state shows the latest changes; the history tool reads further back.
RECENT_LIMIT = 10
# The catalog never changes while the counter runs, so one revision names it.
REVISION = "1"
TOOLS = [
    {
        "name": "increment",
        "description": "Add an integer to the visible counter.",
        "input_schema": {"type": "object", "properties": {"amount": {"type": "integer", "minimum": -100, "maximum": 100}}, "required": ["amount"], "additionalProperties": False},
        "mutates": True,
    },
    {
        "name": "history",
        "description": "Read the last 256 captured counter changes; earlier history is unavailable.",
        "input_schema": {"type": "object", "additionalProperties": False},
        "mutates": False,
    },
]
STATE_LOCK = threading.RLock()
OUTPUT_LOCK = threading.Lock()
value = 0
seq = 0
history = []


class TickError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def send(frame):
    with OUTPUT_LOCK:
        print(json.dumps({"jsonrpc": "2.0", **frame}), flush=True)


def state():
    return {"seq": seq, "data": {"value": value, "recent": history[-RECENT_LIMIT:]}}


def draw():
    if sys.stderr.isatty():
        print(f"\r\033[2KCounter: {value}  |  type + then Enter to change; Ctrl+C exits", file=sys.stderr, flush=True)


def increment(amount):
    global value, seq
    with STATE_LOCK:
        value += amount
        seq += 1
        history.append({"seq": seq, "value": value})
        del history[:-HISTORY_LIMIT]
        send({"method": "tick/update", "params": state()})
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


def call(params):
    if not isinstance(params, dict) or not isinstance(params.get("args", {}), dict):
        raise TickError(-32602, "tick/call parameters and arguments must be objects")
    if params.get("revision") != REVISION:
        raise TickError(-32004, "the Tick Tool catalog changed; describe the Tick and prepare the call again")
    arguments = params.get("args", {})
    if params.get("tool") == "increment":
        amount = arguments.get("amount")
        if set(arguments) != {"amount"} or type(amount) is not int or not -100 <= amount <= 100:
            raise TickError(-32602, "amount must be an integer between -100 and 100")
        return increment(amount)
    if params.get("tool") == "history":
        if arguments:
            raise TickError(-32602, "history accepts no arguments")
        with STATE_LOCK:
            return {"changes": list(history), "earlier_history_unavailable": seq > HISTORY_LIMIT}
    raise TickError(-32602, "unknown Tick Tool")


def respond(method, params):
    if method == "tick/describe":
        return {
            "protocol": 3,
            "kind": "example.counter",
            "title": "Counter",
            "description": "An external nonfinancial Tick. Keyboard and tool calls share one value.",
            "revision": REVISION,
            "recover_with": "history",
            "tools": TOOLS,
            # `recent` grows at its end, so a host that has to shorten it keeps the end.
            "state": {"series": [{"path": "/recent", "newest": "last"}]},
        }
    if method == "tick/read":
        with STATE_LOCK:
            return state()
    if method == "tick/call":
        return call(params)
    raise TickError(-32601, f"unknown method {method!r}")


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
                raise TickError(-32600, "expected a JSON-RPC 2.0 request with an id")
            send({"id": request["id"], "result": respond(request.get("method"), request.get("params", {}))})
        except TickError as error:
            identity = request.get("id") if isinstance(request, dict) else None
            send({"id": identity, "error": {"code": error.code, "message": str(error)}})
        except ValueError as error:
            send({"id": None, "error": {"code": -32700, "message": str(error)}})


if __name__ == "__main__":
    main()
