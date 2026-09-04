#!/usr/bin/env python3
"""Integration test suite for the localhost HTTP server.

Usage:
    cargo build --release
    python3 tests/run_tests.py [path-to-binary]

The script starts the server with config/default.conf, runs every check
against it and reports a summary. It only uses the Python standard library.
"""
import http.client
import os
import socket
import subprocess
import sys
import threading
import time

HOST = "127.0.0.1"
PORT = 8080
PORT2 = 8081

passed = 0
failed = []


def check(name, condition, detail=""):
    global passed
    if condition:
        passed += 1
        print(f"  ok    {name}")
    else:
        failed.append(name)
        print(f"  FAIL  {name}  {detail}")


def request(method, path, body=None, headers=None, port=PORT, host_header=None):
    connection = http.client.HTTPConnection(HOST, port, timeout=5)
    headers = dict(headers or {})
    if host_header:
        headers["Host"] = host_header
    connection.request(method, path, body=body, headers=headers)
    response = connection.getresponse()
    data = response.read()
    connection.close()
    return response, data


def start(binary, config):
    server = subprocess.Popen(
        [binary, config],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(0.6)
    return server


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/release/localhost"

    server = start(binary, "config/default.conf")
    try:
        run_all()
    finally:
        server.terminate()
        server.wait()

    # The configuration-error checks need their own server instance.
    server = start(binary, "config/broken.conf")
    try:
        run_config_error_checks()
    finally:
        server.terminate()
        server.wait()

    print(f"\n{passed} passed, {len(failed)} failed")
    if failed:
        for name in failed:
            print(f"  - {name}")
        sys.exit(1)


def run_all():
    print("[static files]")
    response, data = request("GET", "/")
    check("GET / serves the index file", response.status == 200 and b"localhost" in data)
    check("content-type is html", "text/html" in response.getheader("Content-Type", ""))
    check("responses carry a Date header", bool(response.getheader("Date")))
    check("responses carry a Server header", bool(response.getheader("Server")))
    response, data = request("GET", "/style.css")
    check("GET css with the right mime", response.status == 200
          and response.getheader("Content-Type") == "text/css")
    response, data = request("GET", "/", port=PORT2)
    check("second port serves the same site", response.status == 200 and b"localhost" in data)

    print("[HEAD]")
    response, data = request("HEAD", "/about.html")
    check("HEAD answers 200", response.status == 200)
    check("HEAD sends no body", data == b"")
    check("HEAD still announces the length",
          int(response.getheader("Content-Length", "0")) > 0)

    print("[error handling]")
    response, data = request("GET", "/definitely-not-here")
    check("404 for a missing page", response.status == 404)
    check("custom 404 page is used", b"wrong turn" in data)
    response, data = request("GET", "/%2e%2e/secret")
    check("403 for path traversal", response.status == 403)
    check("custom 403 page is used", b"not allowed to look" in data)
    response, data = request("PUT", "/uploads/x")
    check("405 for a method not allowed", response.status == 405)
    check("405 carries an Allow header", "GET" in (response.getheader("Allow") or ""))
    check("custom 405 page is used", b"does not accept that method" in data)
    response, data = request("POST", "/", body=b"x" * (1048576 + 1),
                             headers={"Content-Type": "text/plain"})
    check("413 for a body over the limit", response.status == 413)
    check("custom 413 page is used", b"client_max_body_size" in data)
    response, data = request("POST", "/", body=b"small body",
                             headers={"Content-Type": "text/plain"})
    check("POST under the limit is accepted", response.status == 200)

    print("[per-server body limits]")
    response, data = request("POST", "/", body=b"tiny", host_header="test.com",
                             headers={"Content-Type": "text/plain"})
    check("vhost accepts a body under its own 1 KiB limit", response.status == 200)
    response, data = request("POST", "/", body=b"y" * 2000, host_header="test.com",
                             headers={"Content-Type": "text/plain"})
    check("vhost rejects a body over its own 1 KiB limit", response.status == 413)
    response, data = request("POST", "/", body=b"y" * 2000,
                             headers={"Content-Type": "text/plain"})
    check("the same body is fine on the default server", response.status == 200)

    print("[bad requests never kill the server]")
    raw = socket.create_connection((HOST, PORT), timeout=5)
    raw.sendall(b"THIS IS NOT HTTP\r\n\r\n")
    answer = raw.recv(4096)
    raw.close()
    check("malformed request answers 400", b"400" in answer.split(b"\r\n")[0])
    check("custom 400 page is used", b"not valid HTTP" in answer)
    raw = socket.create_connection((HOST, PORT), timeout=5)
    raw.sendall(b"GET / HTTP/9.9\r\nHost: localhost\r\n\r\n")
    answer = raw.recv(4096)
    raw.close()
    check("unsupported version answers 400", b"400" in answer.split(b"\r\n")[0])
    response, _ = request("GET", "/")
    check("server still alive afterwards", response.status == 200)

    print("[directory listing and defaults]")
    response, data = request("GET", "/files/")
    check("directory listing renders", response.status == 200 and b"readme.txt" in data)
    check("subdirectories are listed", b"notes/" in data)
    response, data = request("GET", "/files")
    check("directory redirects to a trailing slash",
          response.status == 301 and response.getheader("Location") == "/files/")
    response, data = request("GET", "/files/notes/todo.txt")
    check("nested files are served", response.status == 200 and b"server is done" in data)

    print("[redirections]")
    response, data = request("GET", "/old")
    check("configured redirect answers 301",
          response.status == 301 and response.getheader("Location") == "/about.html")

    print("[uploads: post, get back, delete]")
    payload = os.urandom(4096)
    boundary = "PyTestBoundary42"
    body = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="file"; filename="blob.bin"\r\n'
        f"Content-Type: application/octet-stream\r\n\r\n"
    ).encode() + payload + f"\r\n--{boundary}--\r\n".encode()
    response, data = request("POST", "/uploads/", body=body,
                             headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    check("multipart upload answers 201", response.status == 201)
    response, data = request("GET", "/uploads/blob.bin")
    check("uploaded file comes back intact", response.status == 200 and data == payload)
    response, data = request("DELETE", "/uploads/blob.bin")
    check("DELETE removes the file", response.status == 200)
    response, data = request("GET", "/uploads/blob.bin")
    check("deleted file is now 404", response.status == 404)
    response, data = request("DELETE", "/uploads/blob.bin")
    check("DELETE on a missing file is 404", response.status == 404)
    response, data = request("DELETE", "/files/readme.txt")
    check("DELETE is refused where the route forbids it", response.status == 405)

    print("[cookies and sessions]")
    response, data = request("GET", "/session")
    cookie = response.getheader("Set-Cookie", "")
    check("first visit sets a session cookie", cookie.startswith("SID="))
    sid = cookie.split(";")[0]
    response, data = request("GET", "/session", headers={"Cookie": sid})
    check("session counts visits", b"<b>2</b>" in data)
    check("a known cookie is not reissued", response.getheader("Set-Cookie") is None)
    response, data = request("GET", "/session", headers={"Cookie": sid})
    check("session persists across requests", b"<b>3</b>" in data)
    response, data = request("GET", "/session", headers={"Cookie": "SID=garbage"})
    check("a junk cookie is replaced",
          (response.getheader("Set-Cookie") or "").startswith("SID="))

    print("[cgi]")
    response, data = request("GET", "/cgi/hello.py?name=auditor")
    check("cgi GET runs the script", response.status == 200 and b"Hello, auditor!" in data)
    response, data = request("POST", "/cgi/hello.py", body=b"cgi body here",
                             headers={"Content-Type": "text/plain"})
    check("cgi POST receives the body (unchunked)", b"cgi body here" in data)

    # Chunked request straight through a raw socket.
    raw = socket.create_connection((HOST, PORT), timeout=5)
    raw.sendall(
        b"POST /cgi/hello.py HTTP/1.1\r\nHost: localhost\r\n"
        b"Transfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n"
        b"7\r\nchunked\r\n6\r\n-body!\r\n0\r\n\r\n"
    )
    time.sleep(0.5)
    answer = raw.recv(65536)
    raw.close()
    check("cgi POST receives a chunked body", b"chunked-body!" in answer)
    response, data = request("GET", "/cgi/time.py")
    check("second cgi script works, cwd is the script dir",
          response.status == 200 and b"cgi" in data)
    response, data = request("GET", "/cgi/nope.py")
    check("a missing cgi script is a 404", response.status == 404)

    print("[virtual hosts]")
    response, data = request("GET", "/", host_header="test.com")
    check("Host: test.com routes to the vhost", b"Virtual host" in data)
    response, data = request("GET", "/", host_header="unknown.example")
    check("unknown Host falls back to the default server", b"localhost" in data)

    print("[keep-alive and pipelining]")
    conn = Conn()
    conn.send(b"GET /about.html HTTP/1.1\r\nHost: localhost\r\n\r\n")
    first = conn.status()
    conn.send(b"GET /style.css HTTP/1.1\r\nHost: localhost\r\n\r\n")
    second = conn.status()
    conn.close()
    check("two requests on one connection", b"200" in first and b"200" in second)

    # Both requests in ONE write: the server must answer both without waiting
    # for a read event that will never come.
    conn = Conn()
    conn.send(b"GET /about.html HTTP/1.1\r\nHost: localhost\r\n\r\n"
              b"GET /style.css HTTP/1.1\r\nHost: localhost\r\n\r\n")
    first = conn.status()
    second = conn.status()
    conn.close()
    check("two pipelined requests are both answered",
          b"200" in first and b"200" in second)

    # Three pipelined at once, to be sure it is not a one-off.
    conn = Conn()
    conn.send(b"GET /about.html HTTP/1.1\r\nHost: localhost\r\n\r\n"
              b"GET /style.css HTTP/1.1\r\nHost: localhost\r\n\r\n"
              b"GET /files/readme.txt HTTP/1.1\r\nHost: localhost\r\n\r\n")
    three = [conn.status() for _ in range(3)]
    conn.close()
    check("three pipelined requests are all answered",
          all(b"200" in s for s in three), three)

    conn = Conn()
    conn.send(b"GET /about.html HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
    conn.read()
    check("Connection: close closes the socket", conn.at_eof())
    conn.close()

    print("[availability under load]")
    ok, total, elapsed = concurrent_load(connections=32, per_connection=25)
    rate = ok / total * 100
    check(f"availability {rate:.1f}% over {total} requests on {32} concurrent "
          f"connections ({total / elapsed:.0f} req/s)", rate >= 99.5)
    response, _ = request("GET", "/")
    check("server is still healthy after the load", response.status == 200)


def run_config_error_checks():
    print("[configuration errors]")
    response, data = request("GET", "/", port=9091)
    check("the valid server in a broken config still serves",
          response.status == 200 and b"localhost" in data)
    for port, why in ((9090, "duplicate port inside one server"),
                      (9092, "route without a root")):
        try:
            socket.create_connection((HOST, port), timeout=1).close()
            listening = True
        except OSError:
            listening = False
        check(f"port {port} was rejected ({why})", not listening)


def concurrent_load(connections, per_connection):
    """Hammer the server from several sockets at once, keep-alive style."""
    results = []
    lock = threading.Lock()

    def worker():
        ok = 0
        try:
            conn = Conn()
            for _ in range(per_connection):
                conn.send(b"GET /about.html HTTP/1.1\r\nHost: localhost\r\n\r\n")
                if b"200" in conn.status():
                    ok += 1
            conn.close()
        except OSError:
            pass
        with lock:
            results.append(ok)

    threads = [threading.Thread(target=worker) for _ in range(connections)]
    start_time = time.time()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    elapsed = max(time.time() - start_time, 1e-6)
    return sum(results), connections * per_connection, elapsed


class Conn:
    """A raw connection that reads one response at a time and KEEPS whatever
    arrived after it.

    This matters for pipelining. When two requests are sent in one segment the
    server answers both, and both answers usually arrive in a single recv() --
    measured at 18 times out of 20 on loopback. A reader that returns after the
    first response and throws the rest away would then block forever waiting
    for bytes it had already been given.
    """

    def __init__(self, host=HOST, port=PORT, timeout=5):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.buf = b""

    def send(self, data):
        self.sock.sendall(data)

    def _fill(self):
        chunk = self.sock.recv(8192)
        if not chunk:
            raise ConnectionError("server closed the connection early")
        self.buf += chunk

    def read(self):
        """Return (head, body) for exactly one response."""
        while b"\r\n\r\n" not in self.buf:
            self._fill()
        head, self.buf = self.buf.split(b"\r\n\r\n", 1)
        length = 0
        for line in head.split(b"\r\n"):
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        while len(self.buf) < length:
            self._fill()
        body, self.buf = self.buf[:length], self.buf[length:]
        return head, body

    def status(self):
        return self.read()[0].split(b"\r\n")[0]

    def at_eof(self):
        """True when the server has closed its side and nothing is buffered."""
        if self.buf:
            return False
        try:
            return self.sock.recv(4096) == b""
        except OSError:
            return True

    def close(self):
        self.sock.close()


if __name__ == "__main__":
    main()
