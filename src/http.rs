//! HTTP/1.1 request parsing (Content-Length and chunked bodies) and
//! response building. No allocation-heavy magic, just careful byte work.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    /// Path only, percent-decoded, without the query string.
    pub path: String,
    pub query: String,
    pub version: String,
    /// Header names lowercased.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }

    pub fn keep_alive(&self) -> bool {
        match self.header("connection").map(str::to_ascii_lowercase) {
            Some(value) if value.contains("close") => false,
            Some(value) if value.contains("keep-alive") => true,
            _ => self.version == "HTTP/1.1",
        }
    }

    /// Host header without the port part.
    pub fn hostname(&self) -> Option<String> {
        self.header("host")
            .map(|h| h.split(':').next().unwrap_or("").to_ascii_lowercase())
    }

    pub fn cookies(&self) -> HashMap<String, String> {
        let mut cookies = HashMap::new();
        if let Some(header) = self.header("cookie") {
            for pair in header.split(';') {
                if let Some((name, value)) = pair.trim().split_once('=') {
                    cookies.insert(name.to_string(), value.to_string());
                }
            }
        }
        cookies
    }
}

/// What the parser needs next.
#[derive(Debug, PartialEq)]
pub enum ParseStatus {
    /// Headers are not complete yet.
    NeedMoreHeaders,
    /// Headers parsed; the request needs `total` more body bytes.
    NeedMoreBody,
    /// A full request was parsed, consuming `used` bytes of the buffer.
    Complete { request: Request, used: usize },
    /// The request is malformed beyond repair.
    Bad(&'static str),
}

/// Try to parse one request from the front of `buffer`.
pub fn try_parse(buffer: &[u8]) -> ParseStatus {
    let Some(headers_end) = find_headers_end(buffer) else {
        // Refuse gigantic header sections.
        if buffer.len() > 32 * 1024 {
            return ParseStatus::Bad("header section too large");
        }
        return ParseStatus::NeedMoreHeaders;
    };

    let head = &buffer[..headers_end];
    let Ok(head_text) = std::str::from_utf8(head) else {
        return ParseStatus::Bad("headers are not valid utf-8");
    };

    let mut lines = head_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return ParseStatus::Bad("malformed request line");
    };
    if parts.next().is_some() || method.is_empty() || !target.starts_with('/') {
        return ParseStatus::Bad("malformed request line");
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return ParseStatus::Bad("unsupported protocol version");
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return ParseStatus::Bad("malformed header line");
        };
        headers.insert(
            name.trim().to_ascii_lowercase(),
            value.trim().to_string(),
        );
    }

    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q.to_string()),
        None => (target, String::new()),
    };
    let Some(path) = percent_decode(raw_path) else {
        return ParseStatus::Bad("bad percent-encoding in path");
    };

    let body_start = headers_end + 4;

    // Chunked body?
    let chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);

    if chunked {
        match decode_chunked(&buffer[body_start.min(buffer.len())..]) {
            ChunkStatus::Complete { body, used } => {
                let request = build_request(method, path, query, version, headers, body);
                return ParseStatus::Complete {
                    request,
                    used: body_start + used,
                };
            }
            ChunkStatus::Incomplete => return ParseStatus::NeedMoreBody,
            ChunkStatus::Bad => return ParseStatus::Bad("malformed chunked body"),
        }
    }

    // Content-Length body (or none).
    let content_length = match headers.get("content-length") {
        Some(value) => match value.parse::<usize>() {
            Ok(length) => length,
            Err(_) => return ParseStatus::Bad("invalid content-length"),
        },
        None => 0,
    };

    if buffer.len() < body_start + content_length {
        return ParseStatus::NeedMoreBody;
    }
    let body = buffer[body_start..body_start + content_length].to_vec();
    let request = build_request(method, path, query, version, headers, body);
    ParseStatus::Complete {
        request,
        used: body_start + content_length,
    }
}

fn build_request(
    method: &str,
    path: String,
    query: String,
    version: &str,
    headers: HashMap<String, String>,
    body: Vec<u8>,
) -> Request {
    Request {
        method: method.to_uppercase(),
        path,
        query,
        version: version.to_string(),
        headers,
        body,
    }
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

/// How large could the pending request be? Used for early 413 checks.
pub fn declared_body_size(buffer: &[u8]) -> Option<usize> {
    let headers_end = find_headers_end(buffer)?;
    let head = std::str::from_utf8(&buffer[..headers_end]).ok()?;
    for line in head.split("\r\n").skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok();
            }
        }
    }
    None
}

enum ChunkStatus {
    Complete { body: Vec<u8>, used: usize },
    Incomplete,
    Bad,
}

fn decode_chunked(data: &[u8]) -> ChunkStatus {
    let mut body = Vec::new();
    let mut cursor = 0;

    loop {
        // Chunk size line.
        let Some(line_end) = find_crlf(&data[cursor..]) else {
            return ChunkStatus::Incomplete;
        };
        let line = &data[cursor..cursor + line_end];
        let Ok(text) = std::str::from_utf8(line) else {
            return ChunkStatus::Bad;
        };
        let size_text = text.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            return ChunkStatus::Bad;
        };
        cursor += line_end + 2;

        if size == 0 {
            // Trailer section ends with an empty line.
            let Some(end) = find_crlf(&data[cursor..]) else {
                return ChunkStatus::Incomplete;
            };
            // Either immediately CRLF, or trailers then CRLFCRLF; accept both.
            if end == 0 {
                return ChunkStatus::Complete {
                    body,
                    used: cursor + 2,
                };
            }
            // Skip trailers until the blank line.
            let mut t = cursor;
            loop {
                let Some(line_end) = find_crlf(&data[t..]) else {
                    return ChunkStatus::Incomplete;
                };
                t += line_end + 2;
                if line_end == 0 {
                    return ChunkStatus::Complete { body, used: t };
                }
            }
        }

        if data.len() < cursor + size + 2 {
            return ChunkStatus::Incomplete;
        }
        body.extend_from_slice(&data[cursor..cursor + size]);
        if &data[cursor + size..cursor + size + 2] != b"\r\n" {
            return ChunkStatus::Bad;
        }
        cursor += size + 2;
    }
}

fn find_crlf(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|w| w == b"\r\n")
}

pub fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = *bytes.get(i + 1)? as char;
            let lo = *bytes.get(i + 2)? as char;
            let value = (hi.to_digit(16)? * 16 + lo.to_digit(16)?) as u8;
            out.push(value);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// --------------------------------------------------------------- response

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// `HEAD` answers announce a `Content-Length` but send no body.
    pub suppress_body: bool,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            suppress_body: false,
        }
    }

    pub fn with_body(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        let mut response = Self::new(status);
        response.set_header("Content-Type", content_type);
        response.body = body;
        response
    }

    pub fn html(status: u16, body: String) -> Self {
        Self::with_body(status, "text/html; charset=utf-8", body.into_bytes())
    }

    pub fn set_header(&mut self, name: &str, value: &str) {
        self.headers.push((name.to_string(), value.to_string()));
    }

    pub fn serialize(&self, keep_alive: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 256);
        out.extend_from_slice(
            format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status)).as_bytes(),
        );
        out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        out.extend_from_slice(format!("Date: {}\r\n", http_date()).as_bytes());
        out.extend_from_slice(b"Server: localhost-rs\r\n");
        out.extend_from_slice(if keep_alive {
            b"Connection: keep-alive\r\n" as &[u8]
        } else {
            b"Connection: close\r\n"
        });
        for (name, value) in &self.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        if !self.suppress_body {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

/// Current time as an RFC 9110 IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`.
/// Computed by hand so the project keeps its "std + libc only" promise.
pub fn http_date() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    format_http_date(seconds)
}

fn format_http_date(unix_seconds: u64) -> String {
    const DAY: u64 = 86_400;
    let days = (unix_seconds / DAY) as i64;
    let time_of_day = unix_seconds % DAY;
    let (hour, minute, second) = (time_of_day / 3600, (time_of_day / 60) % 60, time_of_day % 60);

    // 1970-01-01 was a Thursday (index 4 in a Sunday-first week).
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];

    // Howard Hinnant's civil-from-days algorithm.
    let shifted_days = days + 719_468;
    let era = if shifted_days >= 0 { shifted_days } else { shifted_days - 146_096 } / 146_097;
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{weekday}, {day:02} {month_name} {year} {hour:02}:{minute:02}:{second:02} GMT",
        month_name = MONTHS[(month - 1) as usize]
    )
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(raw: &[u8]) -> (Request, usize) {
        match try_parse(raw) {
            ParseStatus::Complete { request, used } => (request, used),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_simple_get() {
        let raw = b"GET /index.html?a=1 HTTP/1.1\r\nHost: example.com:8080\r\n\r\n";
        let (request, used) = complete(raw);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/index.html");
        assert_eq!(request.query, "a=1");
        assert_eq!(request.hostname().as_deref(), Some("example.com"));
        assert!(request.keep_alive());
        assert_eq!(used, raw.len());
    }

    #[test]
    fn waits_for_the_full_body() {
        let raw = b"POST /x HTTP/1.1\r\nContent-Length: 10\r\n\r\n12345";
        assert_eq!(try_parse(raw), ParseStatus::NeedMoreBody);
        let full = b"POST /x HTTP/1.1\r\nContent-Length: 10\r\n\r\n1234567890";
        let (request, _) = complete(full);
        assert_eq!(request.body, b"1234567890");
    }

    #[test]
    fn decodes_a_chunked_body() {
        let raw = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
                    4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let (request, used) = complete(raw);
        assert_eq!(request.body, b"Wikipedia");
        assert_eq!(used, raw.len());
    }

    #[test]
    fn incomplete_chunked_asks_for_more() {
        let raw = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWi";
        assert_eq!(try_parse(raw), ParseStatus::NeedMoreBody);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(try_parse(b"NOT A REQUEST\r\n\r\n"), ParseStatus::Bad(_)));
        assert!(matches!(
            try_parse(b"GET nothing HTTP/1.1\r\n\r\n"),
            ParseStatus::Bad(_)
        ));
        assert!(matches!(
            try_parse(b"GET / FTP/9\r\n\r\n"),
            ParseStatus::Bad(_)
        ));
    }

    #[test]
    fn percent_decoding_works() {
        let raw = b"GET /my%20file.txt HTTP/1.1\r\n\r\n";
        let (request, _) = complete(raw);
        assert_eq!(request.path, "/my file.txt");
    }

    #[test]
    fn cookies_are_parsed() {
        let raw = b"GET / HTTP/1.1\r\nCookie: SID=abc; theme=dark\r\n\r\n";
        let (request, _) = complete(raw);
        let cookies = request.cookies();
        assert_eq!(cookies.get("SID").map(String::as_str), Some("abc"));
        assert_eq!(cookies.get("theme").map(String::as_str), Some("dark"));
    }

    #[test]
    fn keep_alive_rules() {
        let (one, _) = complete(b"GET / HTTP/1.0\r\n\r\n");
        assert!(!one.keep_alive());
        let (two, _) = complete(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(!two.keep_alive());
    }

    #[test]
    fn formats_http_dates_like_rfc_9110() {
        // Reference points cross-checked with `date -u -d @<seconds>`.
        assert_eq!(format_http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(format_http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(format_http_date(1_709_164_800), "Thu, 29 Feb 2024 00:00:00 GMT");
    }

    #[test]
    fn head_answers_announce_a_length_but_send_no_body() {
        let mut response = Response::html(200, "<h1>hey</h1>".into());
        response.suppress_body = true;
        let text = String::from_utf8(response.serialize(true)).unwrap();
        assert!(text.contains("Content-Length: 12\r\n"));
        assert!(text.ends_with("\r\n\r\n"), "no body may follow the headers");
    }

    #[test]
    fn serializes_a_response() {
        let mut response = Response::html(200, "<h1>hey</h1>".into());
        response.set_header("Set-Cookie", "SID=1");
        let bytes = response.serialize(true);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 12\r\n"));
        assert!(text.contains("Connection: keep-alive\r\n"));
        assert!(text.contains("Set-Cookie: SID=1\r\n"));
        assert!(text.contains("Date: "));
        assert!(text.ends_with("<h1>hey</h1>"));
    }
}
