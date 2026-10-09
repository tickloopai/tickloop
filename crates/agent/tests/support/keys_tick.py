# A protocol Tick whose terminal is raw and shows its size and the last bytes it received.
import fcntl, json, os, signal, struct, sys, termios, threading, tty
INSTANCE = os.environ["TICKLOOP_INSTANCE_ID"]
received = []
lock = threading.Lock()
def draw(*_):
    rows, cols, _, _ = struct.unpack("HHHH", fcntl.ioctl(2, termios.TIOCGWINSZ, b"\0" * 8))
    with lock:
        keys = " ".join(f"{byte:02x}" for byte in received[-24:])
    sys.stderr.write(f"\x1b[H\x1b[2Jsize {cols}x{rows}\r\nkeys {keys}\r\n")
    sys.stderr.flush()
def keyboard():
    tty.setraw(2)
    terminal = os.dup(2)
    draw()
    while True:
        data = os.read(terminal, 4096)
        if not data:
            return
        with lock:
            received.extend(data)
        draw()
signal.signal(signal.SIGWINCH, draw)
threading.Thread(target=keyboard, daemon=True).start()
for line in sys.stdin:
    request = json.loads(line)
    if request["method"] == "tick/describe":
        result = {"protocol": 2, "tick_type": "test.keys", "title": "Keys", "description": "Shows the keys it receives", "tools": []}
    else:
        result = {"version": 3, "tick_instance_id": INSTANCE, "initialized": True, "sequence": 0, "observed_at": "2026-10-08T00:00:00Z", "data": {}}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
