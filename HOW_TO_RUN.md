# How to run

## 1. What you need

- **Linux** (or WSL on Windows). The server calls `epoll`, which is a Linux
  system call — it will not build on Windows or macOS.
- **Rust** (any recent stable toolchain). If `cargo` is missing:
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  source "$HOME/.cargo/env"
  ```
- **Python 3** — only for the test scripts and the demo CGI scripts.
- Optional: `siege` for the stress test (`sudo apt install siege`),
  `curl` and `nc` for the manual checks.

Only one crate is pulled in: `libc`, for the raw `epoll` calls.

## 2. Build

```bash
cargo build --release
```

The binary lands in `target/release/localhost`.

> **On Windows, work inside WSL.** Open an Ubuntu shell and go to the project.
> Building through `/mnt/c/...` works, but every file the server reads crosses
> the Windows↔Linux filesystem bridge, which costs milliseconds per request and
> makes the throughput numbers look ~20× worse than they are. For anything you
> plan to measure, copy the project onto the Linux filesystem first:
>
> ```bash
> cp -r /mnt/c/path/to/localhost ~/localhost && cd ~/localhost
> cargo build --release
> ```

## 3. Run

```bash
./target/release/localhost                    # defaults to config/default.conf
./target/release/localhost config/single.conf # or any other config file
```

The server prints what it parsed and what it bound, then serves until you stop
it with `Ctrl-C`:

```
localhost: server 'localhost' on 127.0.0.1:[8080, 8081]
localhost: server 'test.com' on 127.0.0.1:[8080]
localhost: listening on 127.0.0.1:8080
localhost: listening on 127.0.0.1:8081
```

Open <http://127.0.0.1:8080/>. The home page links to every feature: a static
page, a directory listing, an upload form, the session counter, both CGI
scripts, a redirect and a custom 404.

### The configuration files that ship with the project

| File | What it is for |
| ---- | -------------- |
| `config/default.conf` | The full demo: two ports, a virtual host, uploads, CGI, redirects, every custom error page |
| `config/single.conf` | One server, one port — the simplest possible setup |
| `config/ports.conf` | Three servers on three ports, three different roots |
| `config/vhosts.conf` | Two servers sharing `127.0.0.1:8080`, told apart by the `Host` header |
| `config/broken.conf` | Three invalid server blocks and one healthy one, to show error recovery |

## 4. Test

### Unit tests

```bash
cargo test
```

27 tests covering the configuration parser, the HTTP parser and serializer,
the CGI output parser, path-traversal rejection, multipart parsing, the
session store bounds and the HTTP date formatter.

### Integration tests

```bash
cargo build --release
python3 tests/run_tests.py
```

57 end-to-end checks against a live server. The script starts and stops the
server itself, so nothing else needs to be running. It covers static files and
MIME types, both ports, `HEAD`, every custom error page, per-server body
limits, malformed requests, directory listings, redirects, the upload →
download → delete round trip, cookies and sessions, CGI with chunked and
unchunked bodies, virtual hosts, keep-alive, pipelining, concurrent load, and
a second server instance proving `config/broken.conf` still serves.

Expected tail:

```
57 passed, 0 failed
```

### Stress, memory and hanging connections

With siege:

```bash
./target/release/localhost config/default.conf &
siege -b 127.0.0.1:8080/about.html
```

Without siege (same measurements, plus resident memory and slow-client checks):

```bash
./target/release/localhost config/default.conf &
python3 tests/stress.py 30 64        # 30 seconds, 64 concurrent connections
```

```
[load] 123456 served, 0 failed in 30.0s
[load] 4115 req/s, availability 100.00% (siege target: >= 99.5%)
[memory] rss 3200 KiB -> 3200 KiB (peak 3200 KiB)
[memory] flat
```

To watch memory yourself while the load runs:

```bash
top -p $(pgrep -f 'target/release/localhost')
```

## 5. Manual checks

```bash
# static file, headers
curl -i http://127.0.0.1:8080/about.html

# custom error pages
curl -i http://127.0.0.1:8080/nope                      # 404
curl -i -X PUT http://127.0.0.1:8080/uploads/           # 405 + Allow
curl -i 'http://127.0.0.1:8080/%2e%2e/etc/passwd'       # 403

# body limit (the test.com vhost is capped at 1 KiB)
curl -i --resolve test.com:8080:127.0.0.1 -X POST \
     -H "Content-Type: text/plain" --data "short" http://test.com:8080/
curl -i --resolve test.com:8080:127.0.0.1 -X POST \
     -H "Content-Type: text/plain" \
     --data "$(head -c 2000 /dev/zero | tr '\0' 'x')" http://test.com:8080/

# upload, fetch back, delete
head -c 200000 /dev/urandom > /tmp/blob.bin
curl -F "file=@/tmp/blob.bin" http://127.0.0.1:8080/uploads/
curl -s http://127.0.0.1:8080/uploads/blob.bin > /tmp/back.bin && cmp /tmp/blob.bin /tmp/back.bin
curl -i -X DELETE http://127.0.0.1:8080/uploads/blob.bin

# CGI, chunked and unchunked
curl 'http://127.0.0.1:8080/cgi/hello.py?name=auditor'
curl -d "unchunked body" http://127.0.0.1:8080/cgi/hello.py
curl -H "Transfer-Encoding: chunked" -d "chunked body" http://127.0.0.1:8080/cgi/hello.py

# virtual hosts
curl --resolve test.com:8080:127.0.0.1 http://test.com:8080/

# sessions
curl -i http://127.0.0.1:8080/session

# a request that is not HTTP at all — 400, and the server stays up
printf 'THIS IS NOT HTTP\r\n\r\n' | nc 127.0.0.1 8080
curl -i http://127.0.0.1:8080/
```

[AUDIT.md](AUDIT.md) walks through the audit questions one by one with the
command for each.

## 6. Troubleshooting

| Symptom | Cause and fix |
| ------- | ------------- |
| `error[E0432]: unresolved import ... epoll` or the crate fails to build | You are not on Linux. Use WSL. |
| `localhost: cannot bind 127.0.0.1:8080: Address already in use` | Something else holds the port. `ss -tlnp \| grep 8080`, stop it, or change the port in the config. The other listeners still start. |
| `localhost: no valid server in <file>` | Every `server` block was rejected. The error lines printed just above say why. |
| CGI answers 500 | `python3` is not on `PATH`, or the script has a syntax error. Run it by hand: `python3 www/cgi/hello.py`. |
| Uploads answer 500 | `www/uploads/` is not writable. `chmod u+w www/uploads`. |
| Throughput looks bad (a few hundred req/s) | You are running from `/mnt/c` under WSL. See the note in step 2. |
