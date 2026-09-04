//! The event loop: one process, one thread, one epoll instance.
//!
//! Every listener and every client socket is registered with epoll and all
//! reads/writes happen only after epoll reports readiness — never anywhere
//! else. A single `epoll_wait` drives every client/server communication.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use crate::config::ServerConfig;
use crate::handlers::Handler;
use crate::http::{self, ParseStatus, Response};

/// Idle connections are dropped after this many seconds; connections with a
/// half-received request get a 408 first.
const TIMEOUT_SECS: u64 = 15;
const READ_CHUNK: usize = 64 * 1024;
/// Absolute cap while reading, before the per-server limit applies.
const HARD_BODY_CAP: usize = 512 * 1024 * 1024;

struct Listener {
    socket: TcpListener,
    /// Servers answering on this host:port, in configuration order.
    servers: Vec<usize>,
}

enum Phase {
    Reading,
    Writing,
}

struct Connection {
    stream: TcpStream,
    listener: usize,
    read_buffer: Vec<u8>,
    write_buffer: Vec<u8>,
    written: usize,
    phase: Phase,
    keep_alive: bool,
    last_active: Instant,
    /// A request line has been partially received.
    request_pending: bool,
    /// A 408 has already been queued; the next sweep closes the socket.
    timed_out: bool,
}

pub fn run(servers: Vec<ServerConfig>) -> io::Result<()> {
    // Group servers by address; the first server of a group is its default.
    let mut groups: Vec<((String, u16), Vec<usize>)> = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        for &port in &server.ports {
            let key = (server.host.clone(), port);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, list)) => list.push(index),
                None => groups.push((key, vec![index])),
            }
        }
    }

    let mut listeners = Vec::new();
    for ((host, port), group) in groups {
        match TcpListener::bind((host.as_str(), port)) {
            Ok(socket) => {
                socket.set_nonblocking(true)?;
                eprintln!("localhost: listening on {host}:{port}");
                listeners.push(Listener {
                    socket,
                    servers: group,
                });
            }
            Err(error) => {
                // One unusable port must not kill the other listeners.
                eprintln!("localhost: cannot bind {host}:{port}: {error}");
            }
        }
    }
    if listeners.is_empty() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "no port could be bound"));
    }

    let epoll = Epoll::new()?;
    for (index, listener) in listeners.iter().enumerate() {
        epoll.add(listener.socket.as_raw_fd(), index as u64, libc::EPOLLIN as u32)?;
    }

    let listener_groups: Vec<Vec<usize>> =
        listeners.iter().map(|l| l.servers.clone()).collect();
    let mut handler = Handler::new(servers, listener_groups);
    let mut connections: HashMap<u64, Connection> = HashMap::new();
    let mut next_token: u64 = listeners.len() as u64;
    let mut events = vec![unsafe { std::mem::zeroed::<libc::epoll_event>() }; 1024];

    loop {
        let ready = epoll.wait(&mut events, 1000)?;

        for event in &events[..ready] {
            let token = event.u64;
            let flags = event.events;

            if (token as usize) < listeners.len() {
                accept_all(&listeners[token as usize], token as usize, &epoll, &mut connections, &mut next_token);
                continue;
            }

            let Some(connection) = connections.get_mut(&token) else {
                continue;
            };
            connection.last_active = Instant::now();

            let mut drop_connection =
                flags & (libc::EPOLLHUP as u32 | libc::EPOLLERR as u32) != 0;

            if !drop_connection && flags & libc::EPOLLIN as u32 != 0 {
                drop_connection = !readable(connection, &mut handler, &epoll, token);
            }
            if !drop_connection && flags & libc::EPOLLOUT as u32 != 0 {
                drop_connection = !writable(connection, &epoll, token);
                // The client may have pipelined a second request inside the
                // same segment. It is already in the buffer, so answer it now
                // rather than waiting for a read event that will never come.
                if !drop_connection
                    && matches!(connection.phase, Phase::Reading)
                    && !connection.read_buffer.is_empty()
                {
                    drop_connection = !process_buffer(connection, &mut handler, &epoll, token);
                }
            }

            if drop_connection {
                epoll.delete(connection.stream.as_raw_fd());
                connections.remove(&token);
            }
        }

        // Timeout sweep.
        let now = Instant::now();
        let expired: Vec<u64> = connections
            .iter()
            .filter(|(_, c)| now.duration_since(c.last_active).as_secs() >= TIMEOUT_SECS)
            .map(|(&token, _)| token)
            .collect();
        for token in expired {
            let Some(connection) = connections.get_mut(&token) else {
                continue;
            };
            // A client that started a request but never finished it is told
            // so. The 408 is queued and sent by the normal EPOLLOUT path --
            // writes never bypass epoll -- and `timed_out` makes sure the
            // socket is closed on the next sweep even if the client never
            // reads the answer.
            //
            // Only while still reading: a connection that is already sending a
            // response (a 400 to a client that then stopped reading, say) must
            // not have that half-sent answer overwritten by a 408.
            if matches!(connection.phase, Phase::Reading)
                && connection.request_pending
                && !connection.timed_out
            {
                connection.timed_out = true;
                let response =
                    handler.error_response(408, connection.listener, &connection.read_buffer);
                respond(connection, response, false, &epoll, token);
                continue;
            }
            if let Some(connection) = connections.remove(&token) {
                epoll.delete(connection.stream.as_raw_fd());
            }
        }
    }
}

fn accept_all(
    listener: &Listener,
    listener_index: usize,
    epoll: &Epoll,
    connections: &mut HashMap<u64, Connection>,
    next_token: &mut u64,
) {
    loop {
        match listener.socket.accept() {
            Ok((stream, _addr)) => {
                if stream.set_nonblocking(true).is_err() {
                    continue;
                }
                let token = *next_token;
                *next_token += 1;
                if epoll
                    .add(stream.as_raw_fd(), token, libc::EPOLLIN as u32)
                    .is_err()
                {
                    continue;
                }
                connections.insert(
                    token,
                    Connection {
                        stream,
                        listener: listener_index,
                        read_buffer: Vec::new(),
                        write_buffer: Vec::new(),
                        written: 0,
                        phase: Phase::Reading,
                        keep_alive: true,
                        last_active: Instant::now(),
                        request_pending: false,
                        timed_out: false,
                    },
                );
            }
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => return,
            Err(_) => return,
        }
    }
}

/// Handle EPOLLIN: exactly one `read` per readiness event, then hand the
/// buffer to the parser. Returns false when the connection must be dropped.
fn readable(connection: &mut Connection, handler: &mut Handler, epoll: &Epoll, token: u64) -> bool {
    if !matches!(connection.phase, Phase::Reading) {
        return true; // ignore reads while writing a response
    }

    // One read per readiness event.
    let mut chunk = [0u8; READ_CHUNK];
    let received = match connection.stream.read(&mut chunk) {
        Ok(0) => return false, // peer closed
        Ok(received) => received,
        Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => return true,
        Err(_) => return false,
    };
    connection.read_buffer.extend_from_slice(&chunk[..received]);
    process_buffer(connection, handler, epoll, token)
}

/// Parse and answer whatever is already in the read buffer. This performs no
/// I/O at all, so it can safely run outside a readiness event.
fn process_buffer(
    connection: &mut Connection,
    handler: &mut Handler,
    epoll: &Epoll,
    token: u64,
) -> bool {
    if !matches!(connection.phase, Phase::Reading) {
        return true;
    }
    connection.request_pending = !connection.read_buffer.is_empty();

    if connection.read_buffer.len() > HARD_BODY_CAP {
        respond(connection, Response::html(413, "<h1>413 Payload Too Large</h1>".into()), false, epoll, token);
        return true;
    }

    match http::try_parse(&connection.read_buffer) {
        ParseStatus::NeedMoreHeaders => true,
        ParseStatus::NeedMoreBody => {
            // Bodies moderately over the limit are read and discarded so the
            // client cleanly receives its 413 (sent by the handler); only
            // absurdly large declared bodies are cut off right away.
            const EARLY_CUTOFF: usize = 64 * 1024 * 1024;
            if let Some(declared) = http::declared_body_size(&connection.read_buffer) {
                let limit = handler.body_limit(&connection.read_buffer, connection.listener);
                if declared > limit && declared > EARLY_CUTOFF {
                    respond(
                        connection,
                        handler.error_response(413, connection.listener, &connection.read_buffer),
                        false,
                        epoll,
                        token,
                    );
                }
            }
            true
        }
        ParseStatus::Bad(_) => {
            respond(
                connection,
                handler.error_response(400, connection.listener, &connection.read_buffer),
                false,
                epoll,
                token,
            );
            true
        }
        ParseStatus::Complete { request, used } => {
            connection.read_buffer.drain(..used);
            connection.request_pending = false;

            // The handler must never crash the server, whatever happens.
            let keep_alive = request.keep_alive();
            let response = catch_unwind(AssertUnwindSafe(|| {
                handler.handle(&request, connection.listener)
            }))
            .unwrap_or_else(|_| {
                handler.error_response_for(500, &request, connection.listener)
            });

            respond(connection, response, keep_alive, epoll, token);
            true
        }
    }
}

fn respond(
    connection: &mut Connection,
    response: Response,
    keep_alive: bool,
    epoll: &Epoll,
    token: u64,
) {
    connection.write_buffer = response.serialize(keep_alive);
    connection.written = 0;
    connection.keep_alive = keep_alive;
    connection.phase = Phase::Writing;
    let _ = epoll.modify(connection.stream.as_raw_fd(), token, libc::EPOLLOUT as u32);
}

/// Handle EPOLLOUT. Returns false when the connection must be dropped.
fn writable(connection: &mut Connection, epoll: &Epoll, token: u64) -> bool {
    if !matches!(connection.phase, Phase::Writing) {
        return true;
    }

    // One write per readiness event.
    match connection
        .stream
        .write(&connection.write_buffer[connection.written..])
    {
        Ok(sent) => connection.written += sent,
        Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => return true,
        Err(_) => return false,
    }

    if connection.written < connection.write_buffer.len() {
        return true; // wait for the next EPOLLOUT
    }

    if !connection.keep_alive {
        return false;
    }
    // Back to reading the next request on this connection.
    connection.write_buffer.clear();
    connection.written = 0;
    connection.phase = Phase::Reading;
    epoll
        .modify(connection.stream.as_raw_fd(), token, libc::EPOLLIN as u32)
        .is_ok()
}

// ------------------------------------------------------------------ epoll

struct Epoll {
    fd: RawFd,
}

impl Epoll {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::epoll_create1(0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    fn control(&self, op: libc::c_int, fd: RawFd, token: u64, events: u32) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events,
            u64: token,
        };
        let result = unsafe { libc::epoll_ctl(self.fd, op, fd, &mut event) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn add(&self, fd: RawFd, token: u64, events: u32) -> io::Result<()> {
        self.control(libc::EPOLL_CTL_ADD, fd, token, events)
    }

    fn modify(&self, fd: RawFd, token: u64, events: u32) -> io::Result<()> {
        self.control(libc::EPOLL_CTL_MOD, fd, token, events)
    }

    fn delete(&self, fd: RawFd) {
        unsafe {
            libc::epoll_ctl(self.fd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut());
        }
    }

    fn wait(&self, events: &mut [libc::epoll_event], timeout_ms: i32) -> io::Result<usize> {
        let count = unsafe {
            libc::epoll_wait(
                self.fd,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                timeout_ms,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(error);
        }
        Ok(count as usize)
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}
