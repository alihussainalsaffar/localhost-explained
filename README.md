# localhost — explained

An HTTP/1.1 server written in Rust (one process, one thread, one `epoll` instance),
together with a **narrated walk-through of the whole thing** that runs in a browser.

## ▶ Watch the walk-through

**<https://alihussainalsaffar.github.io/localhost-explained/>**

Works on a phone, a tablet or a laptop. It reads itself aloud in English, and every
word spoken is also written on screen, so you can read along with the sound off.

- **22 chapters · 161 slides · about 2 hours**
- Concepts first (what a web server is, HTTP on the wire, sockets, why blocking is
  fatal, how `epoll` works), then **every command with its real captured output**,
  then the code file by file, then the audit questions with answers.
- 20 hand-drawn SVG diagrams for the things prose cannot show.
- Player: play/pause, ±5 s skip, 0.75×–2× speed, voice picker, captions, chapter
  sidebar, scrubbable timeline. **Your position is saved**, so you can stop on the
  train and pick it up later.

| Key | |
| --- | --- |
| `Space` | play / pause |
| `J` `L` | back / forward 5 seconds |
| `←` `→` | previous / next slide |
| `↑` `↓` | previous / next chapter |
| `C` `S` `F` | captions · sidebar · fullscreen |

On a phone: take it off silent, tap **Start** to allow audio, and keep the screen
awake — mobile browsers stop speech when the tab goes to the background. Diagrams
and wide tables scroll sideways; tap **☰** for the chapter list.

## The server itself

| File | What is in it |
| ---- | ------------- |
| [HOW_TO_RUN.md](HOW_TO_RUN.md) | Prerequisites, build, run, test, manual checks, troubleshooting |
| [AUDIT.md](AUDIT.md) | Every audit question with the command that answers it |
| [SUBJECT.md](SUBJECT.md) | The project subject, and where each requirement is implemented |
| [CODE_EXPLAINED.md](CODE_EXPLAINED.md) | The same walk-through in writing, line by line |

```bash
cargo build --release
./target/release/localhost config/default.conf
# then open http://127.0.0.1:8080/
```

Linux only — `epoll` is a Linux system call. On Windows use WSL, and copy the
project onto the Linux filesystem first (running from `/mnt/c` is about 25× slower).

```
main.rs      read the config file, report broken blocks, start the loop
config.rs    tokenize and validate server / route blocks
server.rs    the one epoll instance: accept, read, parse, write, time out
http.rs      request parsing (Content-Length + chunked) and response building
handlers.rs  virtual hosts, routes, files, listings, uploads, sessions, errors
cgi.rs       fork an interpreter, feed it the body, parse what it prints
```

- **GET / POST / DELETE** (plus `HEAD` wherever `GET` is allowed), correct status codes
- Custom error pages for 400, 403, 404, 405, 413, 500, with built-in fallbacks
- File uploads (multipart or raw), fetched back byte-identical, and deleted
- Cookies and sessions, with a store bounded by both time and count
- CGI with chunked and unchunked bodies, `PATH_INFO`, and the script's own cwd
- Virtual hosts, directory listings, index files, redirects, per-server body limits
- Path-traversal protection, request timeouts, keep-alive, HTTP pipelining

**Tests:** 27 unit tests (`cargo test`), 58 integration checks
(`python3 tests/run_tests.py`), and a stress harness (`python3 tests/stress.py 30 64`)
that measures availability, resident memory and slow-client handling.
Measured: 100% availability, ~5000 req/s, RSS flat at ~2 MiB over 100k requests.

## Repository layout

```
index.html          the narrated walk-through (this is what GitHub Pages serves)
src/                the server
www/                the demo site, CGI scripts and error pages
config/             five ready-made configurations
tests/              the integration suite and the stress harness
```
