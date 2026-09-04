# CODE_EXPLAINED.md — the whole server, step by step

This file is **not** committed (it is listed in `.gitignore`). It is your
personal study guide for the audit: the idea behind the project, then every
file walked through in order, with the line numbers as they stand today.

Read it top to bottom once. After that, the section headings alone are enough
to jump straight to whatever the auditor asks about.

---

## Part 0 — The idea in one page

An HTTP server does four things, forever:

1. **Listen.** Ask the kernel for a TCP socket bound to an address and port,
   and mark it as "accepting connections".
2. **Accept.** When a browser connects, the kernel hands you a new socket, one
   per client.
3. **Read a request.** Bytes arrive on that socket. They spell out a request
   line (`GET /index.html HTTP/1.1`), some headers, and maybe a body.
4. **Write a response.** You decide what the request means, then write back a
   status line (`HTTP/1.1 200 OK`), headers, and a body.

The hard part is **step 3 and 4 happening for many clients at the same time,
in one thread.**

If you call `read()` on a socket that has no data yet, the thread **blocks** —
it sits there doing nothing. With one thread and one blocking read, a single
slow client freezes the whole server. Two classic ways out:

- **A thread per client.** Simple, but the subject forbids it, and it does not
  scale to thousands of connections.
- **I/O multiplexing.** Make every socket *non-blocking*, hand all of them to
  the kernel at once, and ask: *"tell me which of these are ready right now."*
  The thread blocks in exactly one place — that question — and never on an
  individual client.

That question is `epoll` on Linux (`select`, `poll` and `kqueue` are the
cousins). This project uses `epoll`, and **one single instance of it**, in
`src/server.rs`. That is the heart of the whole project; everything else is
plumbing around it.

### Why exactly one epoll instance

Because the moment you have two, you have two blocking points, and one can
starve the other. With one:

- `epoll_wait` is the **only** place the process ever sleeps;
- every socket, listeners included, is registered in it;
- no `read` or `write` on a client socket happens anywhere else in the code.

That is precisely what the audit asks you to demonstrate.

### The map of the project

```
main.rs      read the config file, print what broke, start the loop
config.rs    turn the config text into ServerConfig / RouteConfig structs
server.rs    THE EVENT LOOP: accept, read, parse, write, time out
http.rs      bytes  <-> Request / Response
handlers.rs  a parsed Request -> a Response (files, listings, uploads, ...)
cgi.rs       fork an interpreter, feed it the body, read what it prints
```

Data flows in exactly that order: `main → config → server → http → handlers →
cgi`, and the answer flows back out the same way.

---

## Part 1 — `src/main.rs` (44 lines)

Tiny on purpose. It does four things.

**Lines 1–5** — declare the five modules. In Rust, `mod cgi;` means "there is a
file `src/cgi.rs`, compile it as part of this program".

**Lines 10–12** — the config path comes from `argv[1]`, defaulting to
`config/default.conf`:

```rust
let config_path = std::env::args()
    .nth(1)
    .unwrap_or_else(|| "config/default.conf".to_string());
```

`.nth(1)` because `.nth(0)` is the program's own name.

**Lines 14–20** — read the file. If it cannot be read, print to **stderr** and
`exit(1)`. Note this is a plain `read_to_string`: the subject explicitly says
*"There is no need to pass through epoll when reading the configuration file."*

**Lines 22–29** — the important bit:

```rust
let (servers, errors) = config::parse(&source);
for error in &errors {
    eprintln!("localhost: config error (server skipped): {error}");
}
if servers.is_empty() { ... exit(1); }
```

`parse` returns **both** the servers that are fine **and** the errors for the
ones that are not. Every broken block is reported and skipped; the program only
gives up when *nothing* survived. This is the answer to the audit question
*"why should the server work if one of the configurations isn't working?"* —
one typo must not take down every other site on the machine. nginx behaves the
same way.

**Lines 31–43** — print a summary of each server, then hand everything to
`server::run`. If the loop ever returns an error (it only can if no port at all
could be bound), print it and exit.

---

## Part 2 — `src/config.rs` (471 lines) — the configuration file

### The shape of the data (lines 35–105)

Two structs mirror the file format.

```rust
pub struct RouteConfig {          // line 35
    pub prefix: String,           // "/", "/files", "/cgi"
    pub root: Option<String>,     // directory this prefix maps to
    pub index: Option<String>,    // default file when the URL is a directory
    pub methods: Vec<String>,     // accepted methods
    pub redirect: Option<String>, // if set, answer 301 and stop
    pub cgi: HashMap<String, String>,  // ".py" -> "python3"
    pub directory_listing: bool,
    pub upload: bool,
}
```

`Option<String>` is Rust for "this may or may not be set". `root: None` plus
`redirect: None` is invalid and is caught by `validate`.

```rust
pub struct ServerConfig {         // line 70
    pub host: String,
    pub ports: Vec<u16>,          // one server may listen on several ports
    pub server_name: Option<String>,
    pub error_pages: HashMap<u16, String>,  // 404 -> "www/errors/404.html"
    pub max_body_size: usize,
    pub routes: Vec<RouteConfig>,
}
```

Defaults live in `RouteConfig::new` (line 48) and `ServerConfig::new`
(line 80): host `127.0.0.1`, body limit 1 MiB, methods `GET POST DELETE`,
directory listing off, upload off.

### `RouteConfig::allows` (line 61)

```rust
pub fn allows(&self, method: &str) -> bool {
    let effective = if method == "HEAD" { "GET" } else { method };
    self.methods.iter().any(|m| m == method || m == effective)
}
```

`HEAD` is served wherever `GET` is — that is what nginx does, and the audit
compares against nginx. Everything else must be listed explicitly, which is how
`405 Method Not Allowed` happens.

### `ServerConfig::route_for` (line 92) — longest prefix wins

```rust
self.routes.iter()
    .filter(|route| {
        let p = &route.prefix;
        path == p
            || (p == "/" && path.starts_with('/'))
            || (path.starts_with(p.as_str()) && path.as_bytes().get(p.len()) == Some(&b'/'))
    })
    .max_by_key(|route| route.prefix.len())
```

Three ways a route can match:

1. exact equality (`/files` matches the route `/files`);
2. the route is `/`, which matches everything;
3. the path starts with the prefix **and the next character is a `/`**.

That third condition is the subtle one. Without it, the route `/files` would
also swallow `/filesystem`. `path.as_bytes().get(p.len())` looks at exactly the
character after the prefix; `Some(&b'/')` means "and it is a slash".

`max_by_key(prefix.len())` implements *longest match wins*, so with routes `/`
and `/files` defined, `/files/x.txt` goes to `/files`, not to `/`. There is a
unit test for exactly this at line 462.

### `parse` (line 117) — the top level

```rust
let tokens = tokenize(source);
let mut cursor = 0;
while cursor < tokens.len() {
    if tokens[cursor] != "server" { ...error, cursor += 1, continue }
    match parse_server(&tokens, &mut cursor) {
        Ok(server) => match validate(&server).and_then(|()| no_conflict(&servers, &server)) {
            Ok(())     => servers.push(server),
            Err(error) => errors.push(error),
        },
        Err(error) => { errors.push(error); skip_block(&tokens, &mut cursor); }
    }
}
(servers, errors)
```

Read it as a loop over `server { ... }` blocks. Each block is parsed, then
validated on its own, then checked against the blocks already accepted. A
failure at any of those three stages pushes an error and **continues** — it
never returns early. That single design decision is what makes broken
configurations survivable.

`skip_block` (line 314) is the error-recovery step: after a parse failure the
cursor is somewhere random inside a block, so it fast-forwards to the next
`server` keyword and resumes there.

### `tokenize` (line 148)

Splits the file into a flat list of words. Two conveniences:

```rust
let line = raw_line.split('#').next().unwrap_or("");   // strip comments
```

The subject says comments need not be supported; supporting them costs one
line, so the config files can document themselves.

```rust
while let Some(prefix) = rest.strip_prefix('{') { tokens.push("{".into()); rest = prefix; }
```

Braces are split off words, so `route /files {` and `route /files{` both work.

### `parse_server` (line 198) and `parse_route` (line 256)

Both are the same shape: consume `{`, then loop reading a keyword and its
values until `}`.

The `port` case is worth reading (lines 213–232):

```rust
while let Some(word) = tokens.get(*cursor) {
    let Ok(port) = word.parse::<u16>() else { break };
    if server.ports.contains(&port) {
        return Err(ConfigError(format!("port {port} is configured twice in the same server")));
    }
    server.ports.push(port);
    *cursor += 1;
    any = true;
}
```

It keeps eating tokens while they still look like port numbers, which is how
`port 8080 8081` works, and it stops at the first non-number. **The duplicate
check on line 217 is one of the two things the audit asks for** under
*"configure the same port multiple times"*.

An unknown keyword in either block is an error (lines 249, 307), so a typo like
`rooot www/site` is caught instead of silently ignored.

### `validate` (line 320)

Three rules applied to a finished block:

- it must have at least one port;
- it must have at least one route;
- every route needs either a `root` or a `redirect`, otherwise it could not
  answer anything.

### `no_conflict` (line 342) — the *other* duplicate-port case

This is the second half of the audit's port question, and the more interesting
one.

Two `server` blocks are allowed to share a `host:port` — that is exactly what
virtual hosts are. They are only distinguishable if their `server_name`s
differ. If two blocks claim the same address **and** the same name (or neither
has a name), the second one is unreachable, so it is a configuration error:

```rust
let same_name = match (&existing.server_name, &candidate.server_name) {
    (None, None) => true,                                       // two defaults
    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),            // same name
    _ => false,                                                 // one named, one not
};
```

**How to explain it in one sentence:** *"the same port twice is an error when
nothing could ever tell the two servers apart, and legal when a `server_name`
can — that is the difference between a mistake and a virtual host."*

`config/broken.conf` triggers this on purpose. Unit tests at lines 434 and 444
cover both directions.

---

## Part 3 — `src/server.rs` (412 lines) — **the event loop**

This is the file the audit spends most of its time on. Know it cold.

### The `Epoll` wrapper (lines 347–411)

Start at the bottom of the file; everything above uses it.

```rust
struct Epoll { fd: RawFd }
```

`epoll` is a kernel object, referred to by a file descriptor like everything
else in Unix.

**`new` (line 352)** — `libc::epoll_create1(0)` asks the kernel for that
object. Negative return means failure, turned into an `io::Error`. It needs
`unsafe` because it is a raw C call; that is the "you may need unsafe" the
subject mentions.

**`control` (line 360)** — the one function behind add/modify/delete:

```rust
let mut event = libc::epoll_event { events, u64: token };
libc::epoll_ctl(self.fd, op, fd, &mut event)
```

`events` is a bitmask of what you care about: `EPOLLIN` (readable),
`EPOLLOUT` (writable). `u64` is a **token you choose** — the kernel stores it
and hands it back when that fd is ready. This project uses small integers as
tokens: `0..listeners.len()` are the listening sockets, everything above that
is a client. That is how, when an event pops out, you know instantly who it
belongs to (line 100).

**`wait` (line 386)** — the only blocking call in the process:

```rust
let count = libc::epoll_wait(self.fd, events.as_mut_ptr(), events.len() as c_int, timeout_ms);
if count < 0 {
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted { return Ok(0); }   // EINTR
    return Err(error);
}
Ok(count as usize)
```

It fills the caller's array with the ready events and returns how many. The
`EINTR` case matters: a signal can interrupt the syscall, and that is not an
error — you just loop again. Missing this is a classic way to crash a server.

**`Drop` (line 406)** — closes the epoll fd when the struct goes away. Rust's
RAII: no manual cleanup, no leak.

### `Connection` (line 36) — the per-client state

Because you can no longer "wait until the request is complete", you must
**remember** where each client got to:

```rust
struct Connection {
    stream: TcpStream,
    listener: usize,        // which listening socket it arrived on (for vhosts)
    read_buffer: Vec<u8>,   // bytes received so far, not yet parsed
    write_buffer: Vec<u8>,  // the response being sent
    written: usize,         // how much of write_buffer has gone out
    phase: Phase,           // Reading or Writing
    keep_alive: bool,
    last_active: Instant,   // for the timeout sweep
    request_pending: bool,  // a partial request is in the buffer
    timed_out: bool,        // a 408 has already been queued
}
```

`Phase` (line 31) is a two-state machine. A connection is either collecting a
request or sending a response, never both, which is what HTTP/1.1 without
pipelined *responses* means.

### `run` (line 51) — setting up

**Lines 53–64 — grouping.** Several `server` blocks can share one `host:port`.
You must only `bind()` that address once, so servers are grouped by address:

```rust
let mut groups: Vec<((String, u16), Vec<usize>)> = Vec::new();
for (index, server) in servers.iter().enumerate() {
    for &port in &server.ports {
        let key = (server.host.clone(), port);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(index),   // another vhost on this address
            None => groups.push((key, vec![index])),
        }
    }
}
```

A `Vec` rather than a `HashMap` on purpose: it preserves configuration order,
and **the first server in a group is the default** for that address, exactly as
the subject requires.

**Lines 66–86 — binding.**

```rust
match TcpListener::bind((host.as_str(), port)) {
    Ok(socket) => { socket.set_nonblocking(true)?; ... }
    Err(error) => eprintln!("localhost: cannot bind {host}:{port}: {error}"),
}
```

A port that is already taken produces a message and is skipped; the other
listeners still come up. Only if **every** bind fails does `run` return an
error (line 84).

`set_nonblocking(true)` is essential — even `accept()` must never block.

**Lines 88–91 — registering the listeners.** Each listener goes into epoll with
its index as the token and `EPOLLIN` (a listening socket becomes "readable"
when a connection is waiting).

**Lines 93–98 — the loop's state.** `next_token` starts just above the listener
tokens so client tokens can never collide with listener tokens. `events` is a
1024-slot array reused on every iteration — allocated once, never grown.

### `run` (line 100) — the loop itself

```rust
loop {
    let ready = epoll.wait(&mut events, 1000)?;
    for event in &events[..ready] { ... }
    // timeout sweep
}
```

The `1000` is a one-second timeout. Without it the thread would sleep forever
when nothing happens and the timeout sweep would never run. With it, the loop
wakes at least once a second to check for stale connections.

Inside the `for`:

```rust
let token = event.u64;                       // who
let flags = event.events;                    // what happened

if (token as usize) < listeners.len() {      // a listener
    accept_all(...);
    continue;
}
let Some(connection) = connections.get_mut(&token) else { continue };
connection.last_active = Instant::now();
```

Token below `listeners.len()` ⇒ a new connection is waiting. Otherwise it is a
client, and any activity refreshes its idle timer.

```rust
let mut drop_connection = flags & (EPOLLHUP | EPOLLERR) != 0;

if !drop_connection && flags & EPOLLIN != 0 {
    drop_connection = !readable(connection, &mut handler, &epoll, token);
}
if !drop_connection && flags & EPOLLOUT != 0 {
    drop_connection = !writable(connection, &epoll, token);
    if !drop_connection && matches!(connection.phase, Phase::Reading)
        && !connection.read_buffer.is_empty() {
        drop_connection = !process_buffer(connection, &mut handler, &epoll, token);
    }
}

if drop_connection {
    epoll.delete(connection.stream.as_raw_fd());
    connections.remove(&token);
}
```

Four things to notice:

1. **`EPOLLHUP`/`EPOLLERR` first.** The peer hung up or the socket broke; there
   is nothing to read or write, drop it.
2. **`readable` and `writable` both return `bool`** meaning "keep this
   connection". Every I/O error inside them returns `false`, and the block at
   the bottom removes the fd from epoll and drops the `TcpStream`, which closes
   it. *This is the audit's "if an error is returned, is the client removed?"*
3. **The `process_buffer` call after `writable`** handles HTTP pipelining: a
   client may have sent two requests inside one TCP segment. The second is
   already sitting in `read_buffer`, and no further `EPOLLIN` will ever arrive
   for it, so it would hang until the 15 s timeout. `process_buffer` performs
   **no I/O at all** — it only parses bytes already in memory — so this does
   not violate "one read per event".
4. `connections.remove(&token)` drops the `Connection`, which drops its two
   `Vec`s and the `TcpStream`. **That is the whole memory story**: no manual
   free, nothing to leak.

### The timeout sweep (lines 127–166)

```rust
let now = Instant::now();
let expired: Vec<u64> = connections.iter()
    .filter(|(_, c)| now.duration_since(c.last_active).as_secs() >= TIMEOUT_SECS)
    .map(|(&token, _)| token)
    .collect();
```

Collecting into a `Vec` first because you cannot mutate the map while iterating
it.

```rust
for token in expired {
    let Some(connection) = connections.get_mut(&token) else { continue };
    if matches!(connection.phase, Phase::Reading)
        && connection.request_pending && !connection.timed_out {
        connection.timed_out = true;
        let response = handler.error_response(408, connection.listener, &connection.read_buffer);
        respond(connection, response, false, &epoll, token);
        continue;
    }
    if let Some(connection) = connections.remove(&token) {
        epoll.delete(connection.stream.as_raw_fd());
    }
}
```

Two cases:

- **The client started a request and never finished it** (a "slowloris"). It
  gets a `408 Request Timeout`. Crucially the response is **queued** with
  `respond`, which switches the connection to `EPOLLOUT` — the bytes go out
  through the normal write path. Nothing bypasses epoll. `timed_out` makes sure
  that on the next sweep the socket is closed unconditionally, so a client that
  never reads the 408 still cannot linger.
- **Anything else** (an idle keep-alive connection) is simply closed.

The `Phase::Reading` guard is subtle but important: a connection that is
already *sending* a response must not have that half-sent answer overwritten by
a 408. Picture a client that posts garbage, gets a 400 queued, and then stops
reading — without the guard it would receive a truncated 400 with a 408 glued
onto the end. With it, the response it is owed goes out intact and the socket is
simply closed on the next sweep.

`TIMEOUT_SECS = 15` (line 20). This is the audit's *"all requests timeout if
they are taking too long"* and *"no hanging connection"*.

### `accept_all` (line 169)

```rust
loop {
    match listener.socket.accept() {
        Ok((stream, _addr)) => { ...register... }
        Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => return,
        Err(_) => return,
    }
}
```

Loops because one `EPOLLIN` on a listener may mean several pending
connections. `WouldBlock` is the normal exit: it means the backlog is drained.
Each accepted socket is made non-blocking, given the next token, registered
with `EPOLLIN`, and stored with a fresh `Connection`. If `set_nonblocking` or
`epoll.add` fails, the stream is dropped (closed) and the loop continues — a
bad client cannot stop the others being accepted.

### `readable` (line 214) — exactly one read

```rust
if !matches!(connection.phase, Phase::Reading) { return true; }

let mut chunk = [0u8; READ_CHUNK];
let received = match connection.stream.read(&mut chunk) {
    Ok(0) => return false,                                    // peer closed
    Ok(received) => received,
    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return true,
    Err(_) => return false,
};
connection.read_buffer.extend_from_slice(&chunk[..received]);
process_buffer(connection, handler, epoll, token)
```

**One `read` call. No loop.** If the request is bigger than 64 KiB, the rest
arrives on the next `EPOLLIN`. This is the exact code the auditor will read for
*"is there only one read per client per epoll?"*

All four possible outcomes of `read` are handled: `Ok(0)` (orderly close),
`Ok(n)`, `WouldBlock` (spurious wakeup — keep the connection, do nothing), any
other error (drop).

### `process_buffer` (line 233) — parse what we have

Split out of `readable` so it can also run after a write completes. It does no
I/O.

```rust
if connection.read_buffer.len() > HARD_BODY_CAP { ...413...; return true; }
```

An absolute 512 MiB ceiling so a hostile client cannot make the process eat all
of RAM regardless of configuration.

```rust
match http::try_parse(&connection.read_buffer) {
    ParseStatus::NeedMoreHeaders => true,     // wait for more bytes
    ParseStatus::NeedMoreBody    => { ...early 413 check...; true }
    ParseStatus::Bad(_)          => { respond 400; true }
    ParseStatus::Complete { request, used } => { ... }
}
```

The four-way `ParseStatus` is the whole trick of incremental parsing: the
parser is a **pure function of the buffer**, called again every time more bytes
arrive, and it says which of the four situations you are in.

The `NeedMoreBody` branch is a nice detail:

```rust
const EARLY_CUTOFF: usize = 64 * 1024 * 1024;
if let Some(declared) = http::declared_body_size(&connection.read_buffer) {
    let limit = handler.body_limit(&connection.read_buffer, connection.listener);
    if declared > limit && declared > EARLY_CUTOFF { ...413 now... }
}
```

A body slightly over the limit is read to the end and then refused, so the
client receives a clean `413` instead of a connection reset — that is what
`curl` expects. A body announced as *absurdly* large is refused immediately
rather than swallowed. Note `body_limit` peeks at the raw `Host:` header so the
right virtual host's limit applies even before the request is fully parsed.

The `Complete` branch:

```rust
connection.read_buffer.drain(..used);       // remove exactly this request
connection.request_pending = false;

let keep_alive = request.keep_alive();
let response = catch_unwind(AssertUnwindSafe(|| handler.handle(&request, connection.listener)))
    .unwrap_or_else(|_| handler.error_response_for(500, &request, connection.listener));

respond(connection, response, keep_alive, epoll, token);
```

`drain(..used)` removes only the bytes this request consumed — anything after
it is a pipelined request and stays in the buffer.

**`catch_unwind` is the "it never crashes" guarantee.** If any handler panics —
an index out of range, an `unwrap` on `None` — the panic is caught, that one
client gets a `500`, and the server keeps running. Say this sentence in the
audit; it is the direct answer to *"it never crashes"*.

### `respond` (line 299)

```rust
connection.write_buffer = response.serialize(keep_alive);
connection.written = 0;
connection.keep_alive = keep_alive;
connection.phase = Phase::Writing;
let _ = epoll.modify(connection.stream.as_raw_fd(), token, libc::EPOLLOUT as u32);
```

Notice it does **not** write anything. It serializes the response into the
buffer and tells epoll "stop telling me this socket is readable, tell me when
it is writable". The actual write happens later, in `writable`, when epoll says
so. That is the mechanism behind *"writing is ALWAYS done through epoll"*.

### `writable` (line 314) — exactly one write

```rust
match connection.stream.write(&connection.write_buffer[connection.written..]) {
    Ok(sent) => connection.written += sent,
    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return true,
    Err(_) => return false,
}

if connection.written < connection.write_buffer.len() {
    return true;                       // partial write: wait for the next EPOLLOUT
}
if !connection.keep_alive { return false; }        // close

connection.write_buffer.clear();
connection.written = 0;
connection.phase = Phase::Reading;
epoll.modify(connection.stream.as_raw_fd(), token, libc::EPOLLIN as u32).is_ok()
```

**One `write` call, no loop.** A socket buffer that is full accepts only part
of the response; `written` remembers how far it got and the rest goes out on
the next `EPOLLOUT`. This is why a 200 MB file does not block anybody.

When the response is fully sent: close if the client asked for `Connection:
close`, otherwise flip back to `EPOLLIN` and wait for the next request on the
same connection — that is keep-alive.

---

## Part 4 — `src/http.rs` (515 lines) — bytes ⇄ Request/Response

### `Request` (line 7) and its helpers

Header names are stored **lowercased** (line 103), because HTTP header names
are case-insensitive; `header()` (line 19) lowercases the lookup to match.

**`keep_alive` (line 23)** encodes the HTTP/1.0 vs 1.1 difference:

```rust
match self.header("connection").map(str::to_ascii_lowercase) {
    Some(v) if v.contains("close")      => false,
    Some(v) if v.contains("keep-alive") => true,
    _ => self.version == "HTTP/1.1",     // 1.1 defaults to keep-alive, 1.0 does not
}
```

**`hostname` (line 32)** strips the port: `Host: example.com:8080` → `example.com`.
Virtual host matching needs the name only.

**`cookies` (line 37)** splits `Cookie: a=1; b=2` on `;` then on the first `=`.

### `try_parse` (line 64) — the incremental parser

Returns `ParseStatus` (line 52): `NeedMoreHeaders`, `NeedMoreBody`,
`Complete { request, used }`, `Bad(reason)`.

**Step 1 — is the header section complete?**

```rust
let Some(headers_end) = find_headers_end(buffer) else {
    if buffer.len() > 32 * 1024 { return ParseStatus::Bad("header section too large"); }
    return ParseStatus::NeedMoreHeaders;
};
```

`find_headers_end` (line 175) looks for `\r\n\r\n`, the blank line that ends the
headers. The 32 KiB guard stops a client dribbling headers forever to exhaust
memory.

**Step 2 — the request line.**

```rust
let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
else { return ParseStatus::Bad("malformed request line"); };
if parts.next().is_some() || method.is_empty() || !target.starts_with('/') {
    return ParseStatus::Bad("malformed request line");
}
if version != "HTTP/1.1" && version != "HTTP/1.0" {
    return ParseStatus::Bad("unsupported protocol version");
}
```

Exactly three space-separated parts, the target must start with `/`, the
version must be one you speak. `THIS IS NOT HTTP` fails here and becomes a
`400`.

**Step 3 — headers.** Split each line on the first `:`; a line without one is a
`400`.

**Step 4 — the target.** Split on `?` into path and query, then percent-decode
the path (line 149). `percent_decode` (line 258) turns `%20` into a space and
`+` into a space, and returns `None` on a malformed escape (→ `400`). This is
also what makes `/%2e%2e/secret` decode to `/../secret`, which `resolve_path`
in `handlers.rs` then rejects with a `403`.

**Step 5 — the body**, in two flavours.

*Chunked* (lines 133–148): if `Transfer-Encoding: chunked` is present, call
`decode_chunked`.

*Content-Length* (lines 150–156):

```rust
if buffer.len() < body_start + content_length { return ParseStatus::NeedMoreBody; }
let body = buffer[body_start..body_start + content_length].to_vec();
```

The `used` value returned with `Complete` is `body_start + content_length` —
exactly the length of this request, which is what lets the caller `drain` it and
keep any pipelined bytes.

### `decode_chunked` (line 199)

Chunked encoding sends the body as `<size in hex>\r\n<data>\r\n`, repeated,
ending with a `0\r\n\r\n`. It exists so a sender that does not know the total
length up front can start sending immediately.

```rust
let size_text = text.split(';').next().unwrap_or("").trim();   // ignore chunk extensions
let Ok(size) = usize::from_str_radix(size_text, 16) else { return ChunkStatus::Bad };
cursor += line_end + 2;

if size == 0 { ...handle the trailer section, then Complete... }

if data.len() < cursor + size + 2 { return ChunkStatus::Incomplete; }
body.extend_from_slice(&data[cursor..cursor + size]);
if &data[cursor + size..cursor + size + 2] != b"\r\n" { return ChunkStatus::Bad; }
cursor += size + 2;
```

Three points worth making in the audit:

- **`from_str_radix(.., 16)`** — chunk sizes are hexadecimal.
- **`Incomplete` vs `Bad`** — running out of bytes is not an error, it means
  "call me again when more arrive"; a chunk not followed by `\r\n` *is* an error.
- **The `size == 0` branch** handles both `0\r\n\r\n` and a trailer section
  (`0\r\n` followed by more headers and then a blank line).

`4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n` decodes to `Wikipedia`; that is the unit
test at line 434.

### `Response` (line 282) and `serialize` (line 315)

```rust
out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status)).as_bytes());
out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
out.extend_from_slice(format!("Date: {}\r\n", http_date()).as_bytes());
out.extend_from_slice(b"Server: localhost-rs\r\n");
out.extend_from_slice(if keep_alive { b"Connection: keep-alive\r\n" } else { b"Connection: close\r\n" });
for (name, value) in &self.headers { ... }
out.extend_from_slice(b"\r\n");
if !self.suppress_body { out.extend_from_slice(&self.body); }
```

`Content-Length` is computed from the body, never trusted from elsewhere — that
is what lets the client know where the response ends and reuse the connection.

`suppress_body` is the `HEAD` flag: the length is still announced, the bytes are
not sent, exactly as HTTP requires.

### `format_http_date` (line 349)

`Date` is the one header that needs real calendar arithmetic, and the project
may not pull in a date crate. It uses **Howard Hinnant's civil-from-days
algorithm**: the era trick shifts the epoch to 1 March 0000 so leap years form
a clean 400-year (146 097-day) cycle with no special cases. The weekday is
simply `(days + 4) mod 7`, because 1 January 1970 was a Thursday. Three known
timestamps are unit-tested at line 486, including a leap day.

You will probably not be asked about this. If you are: *"it converts a Unix
timestamp to a calendar date without a dependency; the tests pin it against
`date -u`."*

---

## Part 5 — `src/handlers.rs` (645 lines) — request → response

### `Handler` (line 13)

```rust
pub struct Handler {
    servers: Vec<ServerConfig>,
    groups: Vec<Vec<usize>>,                    // listener index -> server indices
    sessions: HashMap<String, (u64, Instant)>,  // session id -> (visits, last seen)
}
```

`groups[listener]` is the list of servers answering on that listening socket,
in configuration order, so `groups[listener][0]` is the default server for that
address.

### Picking the virtual host (`server_for`, line 39; and lines 75–90)

```rust
for &index in group {
    if self.servers[index].server_name.as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(host)) { return &self.servers[index]; }
}
&self.servers[group[0]]     // no match -> the first server for this host:port
```

One `Host:` header, one loop, and the documented fallback. This is the entire
virtual-host mechanism, and it is the answer to the audit's `--resolve`
question.

### `handle` (line 73) — the entry point

1. resolve the virtual host (lines 75–90);
2. `ensure_session` — get or mint the session id;
3. `dispatch` — do the actual work;
4. add `Set-Cookie` if the client did not already have a valid one;
5. set `suppress_body` if the method was `HEAD`.

### Sessions — `ensure_session` (line 453) and `touch_session` (line 112)

This is worth understanding well, because it is where a naive implementation
leaks memory.

```rust
fn ensure_session(request: &Request) -> (String, bool) {
    if let Some(sid) = request.cookies().get("SID") {
        if sid.len() == 32 && sid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return (sid.clone(), false);      // looks like ours, keep it
        }
    }
    (new_session_id(), true)                  // mint one, tell the caller to Set-Cookie
}
```

It is a **free function, not a method**: it touches no shared state. A million
anonymous requests mint a million cookies and store **nothing**. Under `siege`
the resident memory therefore stays flat — which is exactly what the auditor
checks with `top`.

Server-side state is only created when it is actually used, on `/session`:

```rust
fn touch_session(&mut self, session_id: &str) -> u64 {
    let now = Instant::now();
    self.sessions.retain(|_, (_, seen)| now.duration_since(*seen) < SESSION_TTL);  // expire
    if self.sessions.len() >= MAX_SESSIONS && !self.sessions.contains_key(session_id) {
        ...evict the least recently seen...
    }
    let entry = self.sessions.entry(session_id.to_string()).or_insert((0, now));
    entry.0 += 1; entry.1 = now;
    entry.0
}
```

Two independent bounds: **time** (`SESSION_TTL`, 30 minutes, line 26) and
**count** (`MAX_SESSIONS`, 4096, line 28). The store can never grow without
limit no matter what a client does. Unit test at line 627.

`new_session_id` (line 462) reads 16 bytes from `/dev/urandom` and hex-encodes
them (32 characters — which is what the length check above validates), with a
time-based fallback if `/dev/urandom` is somehow unavailable.

### `dispatch` (line 135) — the decision tree

In order:

1. **Body over the server's limit → 413** (line 138). The per-server
   `client_max_body_size`.
2. **`/session`** — the built-in demo page (line 143).
3. **`route_for(path)`** — no route matches → **404** (line 158).
4. **`route.allows(method)`** — no → **405**, with an `Allow` header listing
   the methods that *are* accepted (lines 162–166).
5. **`route.redirect`** — yes → **301** with a `Location` header (line 169).
6. **`resolve_path`** — map the URL onto the filesystem; `None` → **403**
   (line 178).
7. **CGI?** — if the extension matches a configured interpreter, run it
   (lines 181–194). A missing script → 404, an interpreter failure → 500.
8. Otherwise dispatch on the method: `GET`/`HEAD`, `POST`, `DELETE`, anything
   else → **501**.

Reading this function aloud is the fastest way to answer *"is the right status
set for each response?"*

### `resolve_path` (line 341) — the security-critical one

```rust
let rest = if prefix == "/" { url_path.trim_start_matches('/') }
           else { url_path[prefix.len()..].trim_start_matches('/') };

let relative = Path::new(rest);
for component in relative.components() {
    match component {
        Component::Normal(_) | Component::CurDir => {}
        _ => return None,          // "..", absolute paths, drive letters, root
    }
}
Some(Path::new(root).join(relative))
```

First it strips the route prefix, so `/files/notes/todo.txt` under
`root www/files` becomes `www/files/notes/todo.txt`.

Then it walks the path **component by component** and rejects anything that is
not a plain name or `.`. `Component::ParentDir` — a `..` — is rejected, which
is what stops `/../../etc/passwd` (and its percent-encoded form
`/%2e%2e/...`, since decoding happens *before* this check). It returns `None`,
and `dispatch` turns that into a `403`.

**Why component-walking rather than string matching:** a check like
`!path.contains("..")` is defeated by encodings, backslashes and nesting.
Walking parsed components cannot be tricked. Unit test at line 583.

### `get` (line 205)

```rust
if fs_path.is_dir() {
    if !request.path.ends_with('/') { ...301 to path + "/"... }
    if let Some(index) = &route.index {
        let candidate = fs_path.join(index);
        if candidate.is_file() { return serve_file(server, &candidate); }
    }
    if route.directory_listing { return directory_listing(server, &request.path, fs_path); }
    return error_page(server, 403);
}
if fs_path.is_file() { return serve_file(server, fs_path); }
error_page(server, 404)
```

The trailing-slash redirect matters: without it, a relative link like
`<a href="notes/">` on the page at `/files` would resolve against `/`, not
`/files/`, and every link on a listing would break. nginx does the same.

Then: index file if configured and present → listing if enabled → otherwise
403. A directory with neither is forbidden, not "not found", because it does
exist.

### `post` (line 241) — uploads

Two shapes are accepted.

**Browser forms** (`multipart/form-data`, lines 254–279): `multipart_boundary`
(line 479) pulls the boundary out of the `Content-Type`, `save_multipart` does
the parsing, the answer is `201 Created` with links to what was stored.

**Raw bodies** (lines 281–295): `curl --data-binary @file http://.../uploads/x`
writes the body straight to the target path. Also `201`.

If the route does not have `upload on`, a `POST` just acknowledges the payload
with a `200` and the byte count.

### `save_multipart` (line 494)

A multipart body is a sequence of sections separated by `--<boundary>`, each
with its own headers, a blank line, then content, ending at `--<boundary>--`.

```rust
let mut sections = split_bytes(body, delimiter.as_bytes());
sections.next();                                  // the preamble before the first boundary
for section in sections {
    if section.starts_with(b"--") { break; }      // the closing delimiter
    let section = section.strip_prefix(b"\r\n").unwrap_or(section)
                         .strip_suffix(b"\r\n").unwrap_or(section);
    let Some(header_end) = section.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
    ...find filename= in Content-Disposition...
    if let Some(name) = filename { fs::write(target_dir.join(&name), content)?; }
}
```

Two things to point out:

- **It is all `&[u8]`, never `String`.** An uploaded PNG is not valid UTF-8;
  treating it as text would corrupt it. This is why the round-trip test with
  200 KB of `/dev/urandom` passes `cmp`.
- **Only sections with a `filename=` are saved.** Plain form fields are ignored.

`sanitize_filename` (line 543) keeps only the last path segment and only
`[A-Za-z0-9.-_]`, so `../../etc/passwd` becomes `passwd` and cannot escape the
upload directory. Unit test at line 640.

`split_bytes` (line 557) is a small `std::iter::from_fn` generator that splits a
byte slice on a byte-slice separator — the equivalent of `str::split` for raw
bytes.

### `delete` (line 304)

Directory → 403, missing → 404, `fs::remove_file` fails (permissions) → 403,
success → 200. Route-level permission was already checked in `dispatch`, which
is why `DELETE /files/readme.txt` answers 405 and never reaches here.

### `directory_listing` (line 376) and `error_page` (line 407)

The listing reads the directory, appends `/` to subdirectory names, sorts, and
renders a `<ul>` with a `../` link at the top.

`error_page` tries the configured file first and falls back to a built-in page:

```rust
if let Some(page) = server.error_pages.get(&status) {
    if let Ok(content) = fs::read(page) { return Response::with_body(status, "text/html...", content); }
}
Response::html(status, format!("<!DOCTYPE html>...{status} {reason}..."))
```

Note the double fallback: a status with no configured page, *and* a configured
page that cannot be read, both produce the built-in page. **A broken error page
never turns into a broken server.** Worth demonstrating: rename
`www/errors/404.html` and hit a missing URL — you still get a clean 404.

### `mime_type` (line 425)

Extension → `Content-Type`, with `application/octet-stream` as the fallback.
This is what makes the browser render the CSS instead of displaying it.

---

## Part 6 — `src/cgi.rs` (172 lines)

CGI is the 1990s way to make a page dynamic: the server runs a program, gives
it the request through environment variables and standard input, and sends
whatever the program prints back to the client.

### `execute` (line 15)

```rust
let script_absolute = script.canonicalize()?;
let script_dir = script_absolute.parent()...;

let mut child = Command::new(interpreter)
    .arg(&script_absolute)              // "the file to process as first argument"
    .current_dir(&script_dir)           // "pay attention to the directory"
    .env("GATEWAY_INTERFACE", "CGI/1.1")
    .env("REQUEST_METHOD", &request.method)
    .env("QUERY_STRING", &request.query)
    .env("SCRIPT_NAME", &request.path)
    .env("PATH_INFO", script_absolute.as_os_str())   // "the full path"
    .env("CONTENT_LENGTH", request.body.len().to_string())
    .env("CONTENT_TYPE", request.header("content-type").unwrap_or(""))
    .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
    .spawn()?;
```

Three lines map one-to-one onto three sentences of the subject:

- `.arg(&script_absolute)` — *"CGI expects the file to process as first argument"*;
- `.current_dir(&script_dir)` — *"pay attention to the directory where the CGI
  will run for correct relative paths"* (`www/cgi/time.py` prints its `getcwd()`
  so you can show this live);
- `.env("PATH_INFO", ...)` — *"the CGI will check PATH_INFO to define the full path"*.

Then the body:

```rust
if let Some(mut stdin) = child.stdin.take() {
    let _ = stdin.write_all(&request.body);
}
```

`stdin` is **dropped** at the end of that block, which closes the pipe. The
child's `sys.stdin.buffer.read()` then returns. That is the subject's *"EOF as
end of the body"* — and it is the same code path whether the body arrived
chunked or with a `Content-Length`, because `http.rs` has already decoded it
into plain bytes. That is your answer to *"does the CGI work with chunked and
unchunked data?"*

Reading the output (lines 55–76):

```rust
set_nonblocking(&stdout)?;
loop {
    match stdout.read(&mut buffer) {
        Ok(0) => break,                                   // EOF: the child finished
        Ok(received) => output.extend_from_slice(&buffer[..received]),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            if started.elapsed() > CGI_TIMEOUT { child.kill(); ...TimedOut... }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(error) => { child.kill(); return Err(error); }
    }
}
```

The pipe is set non-blocking (line 79, via `fcntl`) and polled with an **8 second
deadline** (`CGI_TIMEOUT`, line 13). A script stuck in an infinite loop is
killed and the client gets a `500` rather than the server hanging forever.

**Be honest about this if asked:** this is the one place the code waits outside
`epoll_wait`, bounded to 8 seconds, and it applies only to CGI requests. The
subject explicitly permits forking for CGI. If you wanted to remove it, you
would register the child's stdout fd in the same epoll instance and keep the
connection in a third `Phase::Cgi` state — say that, it shows you know what the
clean version looks like.

### `parse_cgi_output` (line 93)

A CGI script prints optional headers, a blank line, then the body:

```
Status: 201 Created
Content-Type: text/plain

hello
```

The function finds the first `\r\n\r\n` **or** `\n\n` (Python's `print()`
produces bare `\n`), splits there, and reads the headers. `Status:` sets the
response code; `Content-Type:` sets the type; anything else is passed through as
a response header. If the output has no header block at all, or the "headers"
do not contain a `:`, the whole output is treated as an HTML body — so a script
that just prints HTML still works. Three unit tests at lines 149–171.

---

## Part 7 — the tests

### `cargo test` — 27 unit tests

Pure functions, no sockets. `config.rs`: parsing, duplicate ports both ways,
survival of a broken block, longest-prefix matching, HEAD/GET. `http.rs`:
request parsing, partial bodies, chunked decoding, percent-decoding, cookies,
keep-alive rules, serialization, the date formatter, HEAD suppression.
`handlers.rs`: path traversal, multipart extraction, filename sanitizing,
cookie reuse, the session-store bound. `cgi.rs`: the three output shapes.

### `python3 tests/run_tests.py` — 57 integration checks

Starts a real server on `config/default.conf`, runs everything over real
sockets, then starts a second one on `config/broken.conf` to prove the valid
block still serves. Sections map onto the audit sheet almost one to one.

Two checks use raw sockets rather than `http.client`, because the library will
not produce them: the malformed request (`THIS IS NOT HTTP`) and the chunked
CGI POST.

### `python3 tests/stress.py` — the siege substitute

Concurrent keep-alive load, then availability and req/s, then RSS sampled from
`/proc/<pid>/status` each second (the memory-leak check), then a half-sent
request to show the 408, then 200 abandoned sockets held open while a fresh
connection is still served.

---

## Part 8 — the twelve answers to have ready

1. **How does an HTTP server work?** Listen, accept, read a request, write a
   response. The protocol is text: a request line, headers, a blank line, an
   optional body.

2. **Which multiplexing function, and how?** `epoll`. `epoll_create1` makes the
   object, `epoll_ctl` registers each socket with a token and a set of interests
   (`EPOLLIN`/`EPOLLOUT`), `epoll_wait` blocks until some of them are ready and
   returns only those. `src/server.rs`, lines 347–411.

3. **Only one epoll?** Yes — one `Epoll::new()` and one `epoll_wait` in the
   whole program. `grep -n "epoll_create\|epoll_wait" src/*.rs`.

4. **Why does that matter?** It is the only blocking point in a single-threaded
   process. Two would mean one could starve the other, and any blocking call
   outside it would freeze every client at once.

5. **One read/write per client per epoll event?** Yes. `readable` (line 214)
   does one `read`, `writable` (line 314) does one `write`, neither loops.
   Leftovers come back on the next event.

6. **Are return values checked?** Every `read`/`write` matches on `Ok(0)`,
   `Ok(n)`, `WouldBlock` and other errors. `epoll_ctl`/`epoll_wait` are checked
   for `< 0`, and `EINTR` is treated as "no events", not as a failure.

7. **Is the client removed on error?** Yes — `readable`/`writable` return
   `false`, and the loop calls `epoll.delete(fd)` and removes the connection,
   which drops the `TcpStream` and closes the fd.

8. **Is I/O always through epoll?** For client sockets, yes, including the 408.
   Static files, the config file and the CGI pipe are regular file I/O, which
   the subject allows.

9. **Why does it never crash?** Handlers run inside `catch_unwind`; a panic
   becomes a 500 for one client. Every syscall result is matched, no `unwrap` on
   I/O. Config and bind errors are logged and skipped, never fatal.

10. **Why no memory leak?** Rust frees a `Connection` — buffers and socket —
    when it is removed from the map. The only long-lived collection is the
    session store, which is both time-expired and count-capped. RSS stays around
    3 MiB under sustained load.

11. **Why no hanging connections?** A 1 s tick sweeps for connections idle for
    15 s. A half-sent request gets a 408 first (queued through epoll); anything
    else is closed. `timed_out` guarantees the close happens even if the client
    never reads the 408.

12. **Same port twice — error or virtual host?** Error when nothing could tell
    the two servers apart (same or absent `server_name`) — `no_conflict`,
    `config.rs` line 342 — and also when one block lists a port twice
    (`config.rs` line 217). Legal, and kept, when the `server_name`s differ:
    that is a virtual host.

---

## Part 9 — a five-minute rehearsal

```bash
# 1. one epoll, one wait
grep -n "epoll_create\|epoll_wait" src/*.rs

# 2. one read, one write
sed -n '214,232p;314,346p' src/server.rs

# 3. never crashes
grep -n "catch_unwind" src/server.rs

# 4. config errors are survivable
./target/release/localhost config/broken.conf
curl -i http://127.0.0.1:9091/

# 5. everything else
python3 tests/run_tests.py
```

Then walk the auditor through `AUDIT.md` — it has the command for every
question on the sheet.
