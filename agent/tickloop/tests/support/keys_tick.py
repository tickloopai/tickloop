# An extension Tick whose terminal is raw and shows its size, the last bytes it received and how
# many it received; every byte is also appended to `received` in its working directory.
import fcntl, json, os, signal, struct, sys, termios, threading, tty
received = []
lock = threading.Lock()
def draw(*_):
    rows, cols, _, _ = struct.unpack("HHHH", fcntl.ioctl(2, termios.TIOCGWINSZ, b"\0" * 8))
    with lock:
        keys = " ".join(f"{byte:02x}" for byte in received[-24:])
        count = len(received)
    sys.stderr.write(f"\x1b[H\x1b[2Jsize {cols}x{rows}\r\nkeys {keys}\r\ncount {count}\r\n")
    sys.stderr.flush()
def keyboard():
    tty.setraw(2)
    terminal = os.dup(2)
    draw()
    with open("received", "ab", buffering=0) as log:
        while True:
            data = os.read(terminal, 4096)
            if not data:
                return
            log.write(data)
            with lock:
                received.extend(data)
            draw()
signal.signal(signal.SIGWINCH, draw)
threading.Thread(target=keyboard, daemon=True).start()
for line in sys.stdin:
    request = json.loads(line)
    if request["method"] == "tick/describe":
        result = {"protocol": 3, "revision": "1", "kind": "test.keys", "title": "Keys", "description": "Shows the keys it receives", "tools": []}
    else:
        result = {"seq": 0, "at": "2026-10-08T00:00:00Z", "data": {}}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
