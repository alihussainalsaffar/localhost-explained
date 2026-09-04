#!/usr/bin/env python3
"""Stress and robustness checks for the localhost HTTP server.

This is the stand-in for `siege -b [IP]:[PORT]` when siege is not installed,
plus the two things siege does not measure: resident memory over time and the
handling of slow / abandoned clients.

Usage:
    cargo build --release
    python3 tests/stress.py [seconds] [concurrency]

Defaults: 20 seconds, 64 concurrent keep-alive connections.
"""
import os
import socket
import subprocess
import sys
import threading
import time

HOST = "127.0.0.1"
PORT = 8080
PATH = "/about.html"

stop = threading.Event()
lock = threading.Lock()
served = 0
errors = 0


def read_response(sock):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(8192)
        if not chunk:
            raise OSError("connection closed early")
        data += chunk
    head, rest = data.split(b"\r\n\r\n", 1)
    length = 0
    for line in head.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":")[1])
    while len(rest) < length:
        rest += sock.recv(8192)
    return head


def worker():
    global served, errors
    request = f"GET {PATH} HTTP/1.1\r\nHost: localhost\r\n\r\n".encode()
    sock = None
    while not stop.is_set():
        good = True
        try:
            if sock is None:
                sock = socket.create_connection((HOST, PORT), timeout=5)
            sock.sendall(request)
            head = read_response(sock)
            good = b" 200 " in head.split(b"\r\n")[0]
        except OSError:
            good = False
            if sock is not None:
                sock.close()
            sock = None
        with lock:
            if good:
                served += 1
            else:
                errors += 1
    if sock is not None:
        sock.close()


def resident_kib(pid):
    """RSS of a process, in KiB, straight from /proc (Linux only)."""
    try:
        with open(f"/proc/{pid}/status") as status:
            for line in status:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


def find_server_pid():
    """The listening server may have been started by hand; find it."""
    out = subprocess.run(["pgrep", "-f", "localhost .*conf|/localhost$"],
                         capture_output=True, text=True)
    pids = [int(p) for p in out.stdout.split()]
    return pids[0] if pids else None


def slow_client_checks():
    """A half-sent request must time out, not hold a slot forever."""
    print("\n[slow and abandoned clients]")

    # 1. Send a header block that never ends and wait for the server to react.
    sock = socket.create_connection((HOST, PORT), timeout=40)
    sock.sendall(b"GET / HTTP/1.1\r\nHost: localhost\r\n")  # no blank line
    started = time.time()
    sock.settimeout(40)
    try:
        answer = sock.recv(4096)
    except socket.timeout:
        answer = b""
    waited = time.time() - started
    sock.close()
    status = answer.split(b"\r\n")[0] if answer else b"<nothing, socket closed>"
    print(f"  half-sent request -> {status.decode(errors='replace')} after {waited:.1f}s")
    print("  expected: 408 Request Timeout (or a close) well under 40s")

    # 2. Open sockets and abandon them; the server must not run out of room.
    idle = []
    for _ in range(200):
        try:
            idle.append(socket.create_connection((HOST, PORT), timeout=5))
        except OSError:
            break
    print(f"  opened {len(idle)} idle connections")
    try:
        probe = socket.create_connection((HOST, PORT), timeout=5)
        probe.sendall(f"GET {PATH} HTTP/1.1\r\nHost: localhost\r\n\r\n".encode())
        head = read_response(probe)
        probe.close()
        print(f"  still serving while idle sockets are held: "
              f"{head.split(chr(13).encode())[0].decode(errors='replace')}")
    except OSError as error:
        print(f"  FAILED to serve while idle sockets were held: {error}")
    for sock in idle:
        sock.close()


def main():
    duration = float(sys.argv[1]) if len(sys.argv) > 1 else 20.0
    concurrency = int(sys.argv[2]) if len(sys.argv) > 2 else 64

    pid = find_server_pid()
    if pid is None:
        print("No running server found. Start one first, for example:")
        print("  ./target/release/localhost config/default.conf &")
        sys.exit(1)
    print(f"server pid {pid}, {concurrency} connections, {duration:.0f}s\n")

    before = resident_kib(pid)
    threads = [threading.Thread(target=worker) for _ in range(concurrency)]
    started = time.time()
    for thread in threads:
        thread.start()

    samples = []
    while time.time() - started < duration:
        time.sleep(1.0)
        rss = resident_kib(pid)
        samples.append(rss)
        elapsed = time.time() - started
        with lock:
            done = served
        print(f"  {elapsed:5.1f}s  {done:8d} requests  rss {rss} KiB")

    stop.set()
    for thread in threads:
        thread.join()
    elapsed = time.time() - started
    after = resident_kib(pid)

    total = served + errors
    availability = (served / total * 100) if total else 0.0
    print(f"\n[load] {served} served, {errors} failed in {elapsed:.1f}s")
    print(f"[load] {served / elapsed:.0f} req/s, availability {availability:.2f}% "
          f"(siege target: >= 99.5%)")
    print(f"[memory] rss {before} KiB -> {after} KiB "
          f"(peak {max(samples) if samples else after} KiB)")
    if before and after:
        growth = after - before
        verdict = "flat" if growth <= 512 else f"GREW by {growth} KiB - investigate"
        print(f"[memory] {verdict}")

    slow_client_checks()
    print(f"\nserver still running: {os.path.exists(f'/proc/{pid}')}")


if __name__ == "__main__":
    main()
