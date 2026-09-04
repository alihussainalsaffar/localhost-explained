# Audit — localhost

> Copied for offline reference from the official 01-edu audit page:
> <https://github.com/01-edu/public/tree/master/subjects/localhost/audit>
> Each question is followed by **how to check it on this server**.

Localhost is about creating your own HTTP server and test it with an actual browser.

##### Take the necessary time to understand the project and to test it, looking into the source code will help a lot.

Start the server once and keep it running for the whole audit:

```bash
cargo build --release
./target/release/localhost config/default.conf
```

---

## Functional

**_Is the student able to justify his choices and explain the following:_**
**_Note:_** Ask the student to show you the implementation in the source code when necessary.

### How does an HTTP server works?

It binds a TCP socket, accepts connections, reads a request (a request line,
headers, an optional body), decides what to answer, and writes back a status
line, headers and a body. [src/http.rs](src/http.rs) holds the parser and the
serializer; [src/server.rs](src/server.rs) holds the loop that drives them.

### Which function was used for I/O Multiplexing and how does it works?

`epoll` — `epoll_create1`, `epoll_ctl`, `epoll_wait` — in the `Epoll` struct at
the bottom of [src/server.rs](src/server.rs). Every listener and every client
socket is registered once; `epoll_wait` blocks until at least one of them is
ready, then returns only the ready ones.

### Is the server using only one select (or equivalent) to read the client requests and write answers?

Yes. One `Epoll::new()` in `server::run`, one `epoll_wait` call inside the loop.
`grep -n "epoll_wait\|epoll_create" src/server.rs` shows exactly one of each.

### Why is it important to use only one select and how was it achieved?

One thread cannot block on several sockets at once. A single `epoll_wait` is the
only blocking point in the whole program, so no client can hold up any other.
It was achieved by registering every fd in that one instance and switching a
connection's interest between `EPOLLIN` and `EPOLLOUT` with `epoll_ctl(MOD)`.

### Read the code that goes from the select (or equivalent) to the read and write of a client, is there only one read or write per client per select (or equivalent)?

Yes — `readable()` performs exactly one `read`, `writable()` exactly one `write`,
and neither loops. Partial writes come back on the next `EPOLLOUT`.

### Are the return values for I/O functions checked properly?

Every `read`/`write` is matched on `Ok(0)` (peer closed), `Ok(n)`,
`WouldBlock` (retry later) and any other error. `epoll_ctl`/`epoll_wait`
return values are checked against `< 0` and turned into `io::Error`.

### If an error is returned by the previous functions on a socket, is the client removed?

Yes. `readable`/`writable` return `false` on error, and the event loop then
calls `epoll.delete(fd)` and removes the connection from the map, which drops
the `TcpStream` and closes the fd.

### Is writing and reading ALWAYS done through a select (or equivalent)?

Yes, for every client socket, including the `408` timeout answer, which is
queued and sent by the ordinary `EPOLLOUT` path. Only the configuration file
and the static files/CGI pipe are read outside epoll, which the subject
explicitly allows for the configuration.

---

## Configuration file

Check the configuration file and modify it if necessary.
**_Are the following configurations working properly:_**

### Setup a single server with a single port.

```bash
./target/release/localhost config/single.conf
curl -i http://127.0.0.1:8080/
```

### Setup multiple servers with different port.

```bash
./target/release/localhost config/ports.conf
curl http://127.0.0.1:8080/   # demo site
curl http://127.0.0.1:8081/   # a different root
curl http://127.0.0.1:8082/   # a directory listing
```

### Setup multiple servers with different hostnames (for example: curl --resolve test.com:80:127.0.0.1 http://test.com/). This aims to confirm if your server correctly distinguishes between requests for different hostnames even though they resolve to the same IP and port.

```bash
./target/release/localhost config/vhosts.conf
curl --resolve alpha.com:8080:127.0.0.1 http://alpha.com:8080/
curl --resolve beta.com:8080:127.0.0.1  http://beta.com:8080/
curl -H "Host: nobody.example" http://127.0.0.1:8080/   # falls back to alpha
```

With `config/default.conf` the same applies to `test.com` on port 8080.

### Setup custom error pages.

`config/default.conf` maps 400, 403, 404, 405, 413 and 500 to files in
[www/errors/](www/errors).

```bash
curl -i http://127.0.0.1:8080/nope            # 404, custom page
curl -i -X PUT http://127.0.0.1:8080/uploads/ # 405, custom page + Allow
curl -i 'http://127.0.0.1:8080/%2e%2e/etc'    # 403, custom page
```

Delete or rename a file in `www/errors/` and the built-in page takes over —
the server never fails just because a custom page is missing.

### Limit the client body (for example: curl -X POST -H "Content-Type: plain/text" --data "BODY with something shorter or longer than body limit").

The `test.com` virtual host is capped at 1 KiB on purpose:

```bash
curl -i --resolve test.com:8080:127.0.0.1 -X POST \
     -H "Content-Type: text/plain" --data "short body" http://test.com:8080/
# 200

curl -i --resolve test.com:8080:127.0.0.1 -X POST \
     -H "Content-Type: text/plain" --data "$(head -c 2000 /dev/zero | tr '\0' 'x')" \
     http://test.com:8080/
# 413 with the custom page
```

The default server allows 1 MiB; `head -c 2000000 /dev/zero | tr '\0' x` posted
to `http://127.0.0.1:8080/` returns 413 as well.

### Setup routes and ensure they are taken into account.

`/` → `www/site`, `/files` → `www/files`, `/uploads` → `www/uploads`,
`/cgi` → `www/cgi`, `/old` → a redirect. The longest matching prefix wins
(`ServerConfig::route_for`).

```bash
curl http://127.0.0.1:8080/files/readme.txt
curl -i http://127.0.0.1:8080/old
```

### Setup a default file in case the path is a directory.

`index index.html` on the `/` route:

```bash
curl -i http://127.0.0.1:8080/     # serves www/site/index.html
```

`/files` has no `index`, so it renders a listing instead.

### Setup a list of accepted methods for a route (for example: try to DELETE something with and without permission).

`/uploads` allows `GET POST DELETE`; `/files` allows only `GET`.

```bash
curl -i -X DELETE http://127.0.0.1:8080/files/readme.txt   # 405 + Allow: GET
curl -F "file=@Cargo.toml" http://127.0.0.1:8080/uploads/  # 201
curl -i -X DELETE http://127.0.0.1:8080/uploads/Cargo.toml # 200, file gone
```

---

## Methods and cookies

**_For each method be sure to check the status code (200, 404 etc):_**

### Are the GET requests working properly?

`curl -i http://127.0.0.1:8080/about.html` → 200. A missing file → 404.

### Are the POST requests working properly?

`curl -i -d "hello" http://127.0.0.1:8080/` → 200, and uploads → 201.

### Are the DELETE requests working properly?

See the upload round-trip above: 200 when the file exists, 404 when it does
not, 405 where the route forbids it, 403 on a directory.

### Test a WRONG request, is the server still working properly?

```bash
printf 'THIS IS NOT HTTP\r\n\r\n' | nc 127.0.0.1 8080   # 400 + custom page
printf 'GET / HTTP/9.9\r\nHost: x\r\n\r\n' | nc 127.0.0.1 8080  # 400
curl http://127.0.0.1:8080/                             # still 200
```

### Upload some files to the server and get them back to test they were not corrupted.

The browser form on the home page does this, or:

```bash
head -c 200000 /dev/urandom > /tmp/blob.bin
curl -F "file=@/tmp/blob.bin" http://127.0.0.1:8080/uploads/
curl -s http://127.0.0.1:8080/uploads/blob.bin > /tmp/back.bin
cmp /tmp/blob.bin /tmp/back.bin && echo "identical"
```

The automated suite does the same check with a random 4 KiB payload.

### A working session and cookies system is present on the server?

```bash
curl -i http://127.0.0.1:8080/session          # Set-Cookie: SID=...
curl -s -b "SID=<paste>" http://127.0.0.1:8080/session   # visit counter grows
```

Or open <http://127.0.0.1:8080/session> and reload; the counter increases and
the browser's Application → Cookies panel shows the `SID` cookie.

---

## Interaction with the browser

Open the browser used by the team during tests and its developer tools panel to help you with tests.

### Is the browser connecting with the server with no issues?

<http://127.0.0.1:8080/> serves the demo site, its stylesheet and every linked
page. The Network panel shows 200s and keep-alive connections.

### Are the request and response headers correct? (It should serve a full static website without any problem).

Every answer carries `HTTP/1.1 <code> <reason>`, `Content-Length`, `Date`,
`Server`, `Connection` and a `Content-Type` matched to the file extension.
Compare side by side with nginx in the Network panel.

### Try a wrong URL on the server, is it handled properly?

<http://127.0.0.1:8080/does-not-exist> → the custom 404 page, status 404.

### Try to list a directory, is it handled properly?

<http://127.0.0.1:8080/files/> renders a listing with working links; `/files`
without the slash answers 301 to `/files/` first.

### Try a redirected URL, is it handled properly?

<http://127.0.0.1:8080/old> → 301 to `/about.html`, which the browser follows.

### Check the implemented CGI, does it works properly with chunked and unchunked data?

```bash
curl 'http://127.0.0.1:8080/cgi/hello.py?name=auditor'
curl -d "unchunked body" http://127.0.0.1:8080/cgi/hello.py
curl -H "Transfer-Encoding: chunked" -d "chunked body" http://127.0.0.1:8080/cgi/hello.py
```

The script echoes the byte count it received, so both paths are visible.
`/cgi/time.py` prints its working directory, showing the CGI runs inside the
script's own folder.

---

## Port issues

### Configure multiple ports and websites and ensure it is working as expected.

`config/ports.conf` (see above) — three servers, three ports, three roots.

### Configure the same port multiple times. The server should find the error.

Two forms are detected, both in `config/broken.conf`:

- the same port twice inside one `server` block (`port 9090 9090`);
- the same `host:port` in two blocks that no `server_name` can tell apart.

```bash
./target/release/localhost config/broken.conf
```

```
localhost: config error (server skipped): port 9090 is configured twice in the same server
localhost: config error (server skipped): route '/' needs either a root or a redirect
localhost: config error (server skipped): 127.0.0.1:9091 is already taken by
    server_name '<default>'; a host:port may only be repeated with a different server_name
localhost: listening on 127.0.0.1:9091
```

Sharing a port *with different* `server_name`s is legal and stays legal —
that is what virtual hosts are (`config/vhosts.conf`).

### Configure multiple servers at the same time with different configurations but with common ports. Ask why the server should work if one of the configurations isn't working.

Same run as above: three blocks are rejected, the fourth still listens on 9091
and answers normally (`curl http://127.0.0.1:9091/`). Each `server` block is
parsed and validated on its own; a failure produces a message and a `continue`,
never an exit. Refusing to start at all would mean one typo takes down every
other site the machine hosts, which is exactly what nginx avoids too.

---

## Siege & stress test

### Use siege with a GET method on an empty page, availability should be at least 99.5% with the command `siege -b [IP]:[PORT]`.

```bash
siege -b 127.0.0.1:8080/about.html
```

If siege is not installed, `python3 tests/stress.py 30 64` reports the same
numbers (availability, req/s) plus resident memory.

### Check if there is no memory leak (you could use some tools like top).

`top -p $(pgrep -f target/release/localhost)` while siege runs; RSS settles
around 3–4 MiB and stays there. `tests/stress.py` samples `/proc/<pid>/status`
every second and prints the before/after RSS. The session store, the only
long-lived map in the process, is time-expired and hard-capped
(`SESSION_TTL`, `MAX_SESSIONS` in [src/handlers.rs](src/handlers.rs)).

### Check if there is no hanging connection.

Count the connections the *server* still holds, a few seconds after the load
stops (`TIME_WAIT` entries belong to the client side and always linger):

```bash
ss -tanp state established '( sport = :8080 )'
```

It drains back to nothing. A connection that goes idle for 15 s is dropped, and
one that sent only half a request gets a `408` first. `tests/stress.py`
demonstrates both, and holds 200 abandoned sockets open while the server keeps
answering on a fresh one.

---

## General

### +There's more than one CGI system such as [Python,C++,Perl].

One interpreter (`python3`) with two scripts is implemented. `cgi <.ext>
<interpreter>` accepts any interpreter, so adding one is a config line.

### +There is a second implementation of the server in a different language (repeat practical tests on it before to validate).

Not implemented.
