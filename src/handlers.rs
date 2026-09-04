//! Request handling: virtual hosts, routes, static files, directory
//! listings, uploads, deletions, redirections, sessions and error pages.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cgi;
use crate::config::{RouteConfig, ServerConfig};
use crate::http::{Request, Response};

pub struct Handler {
    servers: Vec<ServerConfig>,
    /// listener index -> indices into `servers` (first one is the default).
    groups: Vec<Vec<usize>>,
    /// session id -> (visits to /session, last time it was seen).
    ///
    /// Only sessions that actually carry server-side state land here, and the
    /// store is swept and capped, so a stress test of a million anonymous
    /// requests cannot make it grow.
    sessions: HashMap<String, (u64, Instant)>,
}

/// A session is forgotten after this much inactivity.
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// Hard ceiling on the session store; the least recently seen entry is evicted.
const MAX_SESSIONS: usize = 4096;

impl Handler {
    pub fn new(servers: Vec<ServerConfig>, groups: Vec<Vec<usize>>) -> Self {
        Self {
            servers,
            groups,
            sessions: HashMap::new(),
        }
    }

    fn server_for(&self, listener: usize, hostname: Option<&str>) -> &ServerConfig {
        let group = &self.groups[listener];
        if let Some(host) = hostname {
            for &index in group {
                if self.servers[index]
                    .server_name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(host))
                {
                    return &self.servers[index];
                }
            }
        }
        &self.servers[group[0]] // the first server for a host:port is default
    }

    /// Body limit for an incoming (not fully parsed) request; scans the raw
    /// header bytes for the Host header to pick the right virtual server.
    pub fn body_limit(&self, raw: &[u8], listener: usize) -> usize {
        let hostname = raw_host(raw);
        self.server_for(listener, hostname.as_deref()).max_body_size
    }

    pub fn error_response(&self, status: u16, listener: usize, raw: &[u8]) -> Response {
        let hostname = raw_host(raw);
        let server = self.server_for(listener, hostname.as_deref());
        error_page(server, status)
    }

    pub fn error_response_for(&self, status: u16, request: &Request, listener: usize) -> Response {
        let server = self.server_for(listener, request.hostname().as_deref());
        error_page(server, status)
    }

    pub fn handle(&mut self, request: &Request, listener: usize) -> Response {
        let hostname = request.hostname();
        let server_index = {
            let group = &self.groups[listener];
            let mut chosen = group[0];
            if let Some(host) = hostname.as_deref() {
                for &index in group {
                    if self.servers[index]
                        .server_name
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(host))
                    {
                        chosen = index;
                        break;
                    }
                }
            }
            chosen
        };

        // Sessions: make sure every client carries a session cookie.
        let (session_id, is_new_session) = ensure_session(request);

        let mut response = self.dispatch(request, server_index, &session_id);
        if is_new_session {
            response.set_header(
                "Set-Cookie",
                &format!("SID={session_id}; Path=/; HttpOnly"),
            );
        }
        // A HEAD answer is the GET answer minus the body.
        if request.method == "HEAD" {
            response.suppress_body = true;
        }
        response
    }

    /// Count one visit for `session_id`, expiring stale sessions and keeping
    /// the store bounded on the way.
    fn touch_session(&mut self, session_id: &str) -> u64 {
        let now = Instant::now();
        self.sessions
            .retain(|_, (_, seen)| now.duration_since(*seen) < SESSION_TTL);
        if self.sessions.len() >= MAX_SESSIONS && !self.sessions.contains_key(session_id) {
            let oldest = self
                .sessions
                .iter()
                .min_by_key(|(_, (_, seen))| *seen)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                self.sessions.remove(&id);
            }
        }
        let entry = self
            .sessions
            .entry(session_id.to_string())
            .or_insert((0, now));
        entry.0 += 1;
        entry.1 = now;
        entry.0
    }

    fn dispatch(&mut self, request: &Request, server_index: usize, session_id: &str) -> Response {
        let server = &self.servers[server_index];

        if request.body.len() > server.max_body_size {
            return error_page(server, 413);
        }

        // Built-in session demo page.
        if request.path == "/session" {
            let count = self.touch_session(session_id);
            return Response::html(
                200,
                format!(
                    "<!DOCTYPE html><html><body><h1>Session</h1>\
                     <p>Your session id: <code>{session_id}</code></p>\
                     <p>You visited this page <b>{count}</b> time(s).</p>\
                     <p><a href=\"/\">home</a></p></body></html>"
                ),
            );
        }

        let server = &self.servers[server_index];
        let Some(route) = server.route_for(&request.path) else {
            return error_page(server, 404);
        };

        if !route.allows(&request.method) {
            let mut response = error_page(server, 405);
            response.set_header("Allow", &route.methods.join(", "));
            return response;
        }

        // Redirection routes.
        if let Some(target) = &route.redirect {
            let mut response = Response::html(
                301,
                format!("<h1>301 Moved Permanently</h1><p><a href=\"{target}\">{target}</a></p>"),
            );
            response.set_header("Location", target);
            return response;
        }

        // Map the URL onto the filesystem, safely.
        let root = route.root.as_deref().unwrap_or(".");
        let Some(fs_path) = resolve_path(root, &route.prefix, &request.path) else {
            return error_page(server, 403);
        };

        // CGI?
        if let Some((interpreter, script)) = cgi_target(route, &fs_path) {
            if !script.is_file() {
                return error_page(server, 404);
            }
            return match cgi::execute(&script, &interpreter, request) {
                Ok(response) => response,
                Err(error) => {
                    eprintln!("localhost: cgi error: {error}");
                    error_page(server, 500)
                }
            };
        }

        match request.method.as_str() {
            "GET" | "HEAD" => self.get(request, server_index, route, &fs_path),
            "POST" => self.post(request, server_index, route, &fs_path),
            "DELETE" => self.delete(server_index, &fs_path),
            _ => error_page(server, 501),
        }
    }

    fn get(
        &self,
        request: &Request,
        server_index: usize,
        route: &RouteConfig,
        fs_path: &Path,
    ) -> Response {
        let server = &self.servers[server_index];

        if fs_path.is_dir() {
            // Directories are canonicalized to a trailing slash.
            if !request.path.ends_with('/') {
                let target = format!("{}/", request.path);
                let mut response = Response::html(301, String::new());
                response.set_header("Location", &target);
                return response;
            }
            // Default file for directory requests.
            if let Some(index) = &route.index {
                let candidate = fs_path.join(index);
                if candidate.is_file() {
                    return serve_file(server, &candidate);
                }
            }
            if route.directory_listing {
                return directory_listing(server, &request.path, fs_path);
            }
            return error_page(server, 403);
        }

        if fs_path.is_file() {
            return serve_file(server, fs_path);
        }
        error_page(server, 404)
    }

    fn post(
        &self,
        request: &Request,
        server_index: usize,
        route: &RouteConfig,
        fs_path: &Path,
    ) -> Response {
        let server = &self.servers[server_index];

        if route.upload {
            let target_dir = if fs_path.is_dir() {
                fs_path.to_path_buf()
            } else {
                fs_path.parent().map(Path::to_path_buf).unwrap_or_default()
            };

            // Browser form upload: multipart/form-data.
            let content_type = request.header("content-type").unwrap_or("");
            if let Some(boundary) = multipart_boundary(content_type) {
                return match save_multipart(&request.body, &boundary, &target_dir) {
                    Ok(saved) if !saved.is_empty() => {
                        let list: Vec<String> = saved
                            .iter()
                            .map(|name| format!("<li><a href=\"{name}\">{name}</a></li>"))
                            .collect();
                        Response::html(
                            201,
                            format!(
                                "<h1>201 Created</h1><ul>{}</ul><p><a href=\"./\">back</a></p>",
                                list.join("")
                            ),
                        )
                    }
                    Ok(_) => Response::html(200, "<h1>No file in the upload</h1>".into()),
                    Err(error) => {
                        eprintln!("localhost: upload error: {error}");
                        error_page(server, 500)
                    }
                };
            }

            // Raw body upload straight to the URL target (curl --data-binary).
            if !fs_path.is_dir() {
                return match fs::write(fs_path, &request.body) {
                    Ok(()) => Response::html(
                        201,
                        format!("<h1>201 Created</h1><p>{} bytes stored.</p>", request.body.len()),
                    ),
                    Err(error) => {
                        eprintln!("localhost: upload error: {error}");
                        error_page(server, 500)
                    }
                };
            }
        }

        // Generic POST: acknowledge the payload.
        Response::html(
            200,
            format!("<h1>POST received</h1><p>{} bytes.</p>", request.body.len()),
        )
    }

    fn delete(&self, server_index: usize, fs_path: &Path) -> Response {
        let server = &self.servers[server_index];
        if fs_path.is_dir() {
            return error_page(server, 403);
        }
        if !fs_path.is_file() {
            return error_page(server, 404);
        }
        match fs::remove_file(fs_path) {
            Ok(()) => Response::html(200, "<h1>Deleted</h1>".into()),
            Err(_) => error_page(server, 403),
        }
    }
}

// ------------------------------------------------------------------ util

/// Extract the Host header from raw (possibly incomplete) header bytes.
fn raw_host(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("host") {
                return Some(
                    value.trim().split(':').next().unwrap_or("").to_ascii_lowercase(),
                );
            }
        }
    }
    None
}

/// Join the URL path (minus the route prefix) onto the route root while
/// refusing any traversal outside of it.
fn resolve_path(root: &str, prefix: &str, url_path: &str) -> Option<PathBuf> {
    let rest = if prefix == "/" {
        url_path.trim_start_matches('/')
    } else {
        url_path[prefix.len()..].trim_start_matches('/')
    };

    let relative = Path::new(rest);
    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return None, // "..", absolute paths, drive letters...
        }
    }
    Some(Path::new(root).join(relative))
}

fn cgi_target(route: &RouteConfig, fs_path: &Path) -> Option<(String, PathBuf)> {
    let extension = format!(
        ".{}",
        fs_path.extension().and_then(|e| e.to_str()).unwrap_or("")
    );
    route
        .cgi
        .get(&extension)
        .map(|interpreter| (interpreter.clone(), fs_path.to_path_buf()))
}

fn serve_file(server: &ServerConfig, path: &Path) -> Response {
    match fs::read(path) {
        Ok(content) => Response::with_body(200, mime_type(path), content),
        Err(_) => error_page(server, 403),
    }
}

fn directory_listing(server: &ServerConfig, url_path: &str, dir: &Path) -> Response {
    let Ok(entries) = fs::read_dir(dir) else {
        return error_page(server, 403);
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() {
                name.push('/');
            }
            name
        })
        .collect();
    names.sort();

    let rows: Vec<String> = names
        .iter()
        .map(|name| format!("<li><a href=\"{name}\">{name}</a></li>"))
        .collect();
    Response::html(
        200,
        format!(
            "<!DOCTYPE html><html><head><title>Index of {url_path}</title></head>\
             <body><h1>Index of {url_path}</h1><ul><li><a href=\"../\">../</a></li>{}</ul>\
             </body></html>",
            rows.join("")
        ),
    )
}

fn error_page(server: &ServerConfig, status: u16) -> Response {
    if let Some(page) = server.error_pages.get(&status) {
        if let Ok(content) = fs::read(page) {
            return Response::with_body(status, "text/html; charset=utf-8", content);
        }
    }
    Response::html(
        status,
        format!(
            "<!DOCTYPE html><html><head><title>{status} {reason}</title></head>\
             <body style=\"font-family:sans-serif;text-align:center;margin-top:15vh\">\
             <h1>{status} {reason}</h1><hr style=\"width:12rem\">\
             <p>localhost-rs</p></body></html>",
            reason = crate::http::reason(status)
        ),
    )
}

fn mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "ppm" => "image/x-portable-pixmap",
        _ => "application/octet-stream",
    }
}

/// Reuse the client's `SID` cookie when it looks like one of ours, otherwise
/// mint a fresh id. Nothing is written to the session store here: anonymous
/// traffic must not cost the server any memory.
fn ensure_session(request: &Request) -> (String, bool) {
    if let Some(sid) = request.cookies().get("SID") {
        if sid.len() == 32 && sid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return (sid.clone(), false);
        }
    }
    (new_session_id(), true)
}

fn new_session_id() -> String {
    // 16 random bytes from /dev/urandom are plenty for a session cookie.
    use std::io::Read;
    let mut bytes = [0u8; 16];
    match fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)) {
        Ok(()) => {}
        Err(_) => {
            // Extremely unlikely fallback: time-based.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            bytes.copy_from_slice(&now.as_nanos().to_le_bytes());
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    if !content_type.starts_with("multipart/form-data") {
        return None;
    }
    for part in content_type.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("boundary=") {
            return Some(value.trim_matches('"').to_string());
        }
    }
    None
}

/// Minimal multipart/form-data parser: saves every part that has a
/// filename into `target_dir` and returns the stored names.
fn save_multipart(body: &[u8], boundary: &str, target_dir: &Path) -> std::io::Result<Vec<String>> {
    let delimiter = format!("--{boundary}");
    let mut saved = Vec::new();

    let mut sections = split_bytes(body, delimiter.as_bytes());
    sections.next(); // preamble before the first boundary
    for section in sections {
        // The closing delimiter section starts with "--".
        if section.starts_with(b"--") {
            break;
        }
        // Strip the leading CRLF and the trailing CRLF.
        let section = section
            .strip_prefix(b"\r\n")
            .unwrap_or(section)
            .strip_suffix(b"\r\n")
            .unwrap_or(section);

        let Some(header_end) = section.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&section[..header_end]);
        let content = &section[header_end + 4..];

        let mut filename = None;
        for line in headers.split("\r\n") {
            if line.to_ascii_lowercase().starts_with("content-disposition") {
                for attribute in line.split(';') {
                    let attribute = attribute.trim();
                    if let Some(value) = attribute.strip_prefix("filename=") {
                        let name = value.trim_matches('"');
                        if !name.is_empty() {
                            filename = Some(sanitize_filename(name));
                        }
                    }
                }
            }
        }

        if let Some(name) = filename {
            fs::create_dir_all(target_dir)?;
            fs::write(target_dir.join(&name), content)?;
            saved.push(name);
        }
    }
    Ok(saved)
}

/// Keep only the base name; no directories, no weird characters.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("upload");
    let clean: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if clean.is_empty() {
        "upload".to_string()
    } else {
        clean
    }
}

/// Iterator over the pieces of `data` separated by `sep`.
fn split_bytes<'a>(data: &'a [u8], sep: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    let mut rest = Some(data);
    std::iter::from_fn(move || {
        let current = rest?;
        match current
            .windows(sep.len().max(1))
            .position(|window| window == sep)
        {
            Some(position) => {
                let (piece, tail) = current.split_at(position);
                rest = Some(&tail[sep.len()..]);
                Some(piece)
            }
            None => {
                rest = None;
                Some(current)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_traversal_is_rejected() {
        assert!(resolve_path("www", "/", "/../secret").is_none());
        assert!(resolve_path("www", "/files", "/files/../../etc/passwd").is_none());
        assert_eq!(
            resolve_path("www", "/files", "/files/a/b.txt"),
            Some(PathBuf::from("www/a/b.txt"))
        );
    }

    #[test]
    fn multipart_files_are_extracted() {
        let boundary = "XBOUND";
        let body = b"--XBOUND\r\n\
            Content-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\n\
            Content-Type: text/plain\r\n\r\n\
            hello world\r\n\
            --XBOUND--\r\n";
        let dir = std::env::temp_dir().join("localhost-mp-test");
        let _ = fs::remove_dir_all(&dir);
        let saved = save_multipart(body, boundary, &dir).unwrap();
        assert_eq!(saved, vec!["hello.txt"]);
        assert_eq!(fs::read(dir.join("hello.txt")).unwrap(), b"hello world");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_valid_cookie_is_reused_and_a_junk_one_replaced() {
        let with_cookie = |cookie: &str| crate::http::Request {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            headers: HashMap::from([("cookie".to_string(), cookie.to_string())]),
            body: Vec::new(),
        };
        let good = "0123456789abcdef0123456789abcdef";
        let (id, is_new) = ensure_session(&with_cookie(&format!("SID={good}")));
        assert_eq!(id, good);
        assert!(!is_new, "a well-formed session cookie must be kept");
        let (id, is_new) = ensure_session(&with_cookie("SID=not-a-session"));
        assert!(is_new && id.len() == 32, "junk cookies are replaced");
    }

    #[test]
    fn the_session_store_stays_bounded() {
        let mut handler = Handler::new(Vec::new(), Vec::new());
        for index in 0..MAX_SESSIONS + 100 {
            handler.touch_session(&format!("{index:032x}"));
        }
        assert!(
            handler.sessions.len() <= MAX_SESSIONS,
            "store grew to {}",
            handler.sessions.len()
        );
    }

    #[test]
    fn filenames_are_sanitized() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\evil\\name.txt"), "name.txt");
        assert_eq!(sanitize_filename("<>::"), "upload");
    }
}
