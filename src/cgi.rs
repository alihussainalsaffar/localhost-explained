//! CGI execution. The server forks a child process (the interpreter), feeds
//! it the request body on stdin (EOF marks the end of the body) and parses
//! the produced headers/body. The child runs in the script's directory so
//! relative paths inside the script behave.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::http::{Request, Response};

const CGI_TIMEOUT: Duration = Duration::from_secs(8);

pub fn execute(script: &Path, interpreter: &str, request: &Request) -> io::Result<Response> {
    let script_absolute = script.canonicalize()?;
    let script_dir = script_absolute
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ".".into());

    let mut child = Command::new(interpreter)
        // The CGI expects the file to process as its first argument.
        .arg(&script_absolute)
        .current_dir(&script_dir)
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("REQUEST_METHOD", &request.method)
        .env("QUERY_STRING", &request.query)
        .env("SCRIPT_NAME", &request.path)
        // The CGI checks PATH_INFO to know the full path of the file.
        .env("PATH_INFO", script_absolute.as_os_str())
        .env("CONTENT_LENGTH", request.body.len().to_string())
        .env(
            "CONTENT_TYPE",
            request.header("content-type").unwrap_or(""),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    // Body -> stdin, then close it: EOF is the end of the body.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&request.body);
    }

    // Read stdout without blocking forever: poll the child with a deadline.
    let mut stdout = child.stdout.take().expect("stdout was piped");
    set_nonblocking(&stdout)?;

    let mut output = Vec::new();
    let started = Instant::now();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => break, // EOF: the child closed stdout
            Ok(received) => output.extend_from_slice(&buffer[..received]),
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() > CGI_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "cgi timed out"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
    let _ = child.wait();

    Ok(parse_cgi_output(output))
}

fn set_nonblocking(stdout: &std::process::ChildStdout) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// CGI output is optional headers, a blank line, then the body.
fn parse_cgi_output(output: Vec<u8>) -> Response {
    let split = output
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4))
        .or_else(|| output.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2)));

    let Some((header_end, separator)) = split else {
        // No header block: treat everything as html.
        return Response::with_body(200, "text/html; charset=utf-8", output);
    };

    let header_text = String::from_utf8_lossy(&output[..header_end]).into_owned();
    let body = output[header_end + separator..].to_vec();

    let mut status = 200;
    let mut content_type = "text/html; charset=utf-8".to_string();
    let mut extra_headers = Vec::new();
    let mut any_header = false;

    for line in header_text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            // Not header-shaped: the "headers" were actually body text.
            return Response::with_body(200, "text/html; charset=utf-8", output);
        };
        any_header = true;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            status = value
                .split(' ')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(200);
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = value.to_string();
        } else {
            extra_headers.push((name.to_string(), value.to_string()));
        }
    }
    if !any_header {
        return Response::with_body(200, "text/html; charset=utf-8", output);
    }

    let mut response = Response::with_body(status, &content_type, body);
    for (name, value) in extra_headers {
        response.set_header(&name, &value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cgi_headers_and_body() {
        let output = b"Status: 201 Created\r\nContent-Type: text/plain\r\nX-Extra: 1\r\n\r\nhello".to_vec();
        let response = parse_cgi_output(output);
        assert_eq!(response.status, 201);
        assert_eq!(response.body, b"hello");
        assert!(response
            .headers
            .iter()
            .any(|(n, v)| n == "X-Extra" && v == "1"));
    }

    #[test]
    fn headerless_output_becomes_the_body() {
        let response = parse_cgi_output(b"<h1>raw</h1>".to_vec());
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"<h1>raw</h1>");
    }

    #[test]
    fn newline_only_separator_is_accepted() {
        let response = parse_cgi_output(b"Content-Type: text/plain\n\nbody".to_vec());
        assert_eq!(response.body, b"body");
    }
}
