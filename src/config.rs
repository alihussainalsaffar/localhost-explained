//! Configuration file parsing and validation.
//!
//! Format (nginx-flavoured, no comments needed):
//!
//! ```text
//! server {
//!     host 127.0.0.1
//!     port 8080 8081
//!     server_name example.com
//!     error_page 404 www/errors/404.html
//!     client_max_body_size 1048576
//!     route / {
//!         root www/site
//!         index index.html
//!         methods GET POST DELETE
//!         directory_listing on
//!     }
//!     route /old {
//!         redirect /new
//!     }
//!     route /cgi {
//!         root www/cgi
//!         cgi .py python3
//!     }
//! }
//! ```
//!
//! A broken `server` block is reported and skipped so one bad
//! configuration never brings down the other servers.

use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone)]
pub struct RouteConfig {
    pub prefix: String,
    pub root: Option<String>,
    pub index: Option<String>,
    pub methods: Vec<String>,
    pub redirect: Option<String>,
    /// extension -> interpreter, e.g. ".py" -> "python3"
    pub cgi: HashMap<String, String>,
    pub directory_listing: bool,
    pub upload: bool,
}

impl RouteConfig {
    fn new(prefix: String) -> Self {
        Self {
            prefix,
            root: None,
            index: None,
            methods: vec!["GET".into(), "POST".into(), "DELETE".into()],
            redirect: None,
            cgi: HashMap::new(),
            directory_listing: false,
            upload: false,
        }
    }

    pub fn allows(&self, method: &str) -> bool {
        // HEAD is served wherever GET is, exactly like nginx: the answer is
        // the GET answer with the body left out.
        let effective = if method == "HEAD" { "GET" } else { method };
        self.methods.iter().any(|m| m == method || m == effective)
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub ports: Vec<u16>,
    pub server_name: Option<String>,
    pub error_pages: HashMap<u16, String>,
    pub max_body_size: usize,
    pub routes: Vec<RouteConfig>,
}

impl ServerConfig {
    fn new() -> Self {
        Self {
            host: "127.0.0.1".into(),
            ports: Vec::new(),
            server_name: None,
            error_pages: HashMap::new(),
            max_body_size: 1024 * 1024, // 1 MiB default
            routes: Vec::new(),
        }
    }

    /// Longest matching route prefix wins.
    pub fn route_for(&self, path: &str) -> Option<&RouteConfig> {
        self.routes
            .iter()
            .filter(|route| {
                let p = &route.prefix;
                path == p
                    || (p == "/" && path.starts_with('/'))
                    || (path.starts_with(p.as_str())
                        && path.as_bytes().get(p.len()) == Some(&b'/'))
            })
            .max_by_key(|route| route.prefix.len())
    }
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Parse a whole file. Invalid server blocks are returned as errors next to
/// the valid servers so the caller can log them and keep going.
pub fn parse(source: &str) -> (Vec<ServerConfig>, Vec<ConfigError>) {
    let tokens = tokenize(source);
    let mut cursor = 0;
    let mut servers = Vec::new();
    let mut errors = Vec::new();

    while cursor < tokens.len() {
        if tokens[cursor] != "server" {
            errors.push(ConfigError(format!(
                "expected 'server', found '{}'",
                tokens[cursor]
            )));
            // Skip to the next plausible block start.
            cursor += 1;
            continue;
        }
        match parse_server(&tokens, &mut cursor) {
            Ok(server) => match validate(&server).and_then(|()| no_conflict(&servers, &server)) {
                Ok(()) => servers.push(server),
                Err(error) => errors.push(error),
            },
            Err(error) => {
                errors.push(error);
                // Resynchronize: skip past the closing brace of this block.
                skip_block(&tokens, &mut cursor);
            }
        }
    }
    (servers, errors)
}

fn tokenize(source: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for raw_line in source.lines() {
        // '#' comments are tolerated even though the subject doesn't ask.
        let line = raw_line.split('#').next().unwrap_or("");
        for word in line.split_whitespace() {
            // Braces may be glued to words.
            let mut rest = word;
            while let Some(prefix) = rest.strip_prefix('{') {
                tokens.push("{".into());
                rest = prefix;
            }
            let mut suffixes = 0;
            while let Some(prefix) = rest.strip_suffix('}') {
                suffixes += 1;
                rest = prefix;
            }
            if !rest.is_empty() {
                tokens.push(rest.to_string());
            }
            for _ in 0..suffixes {
                tokens.push("}".into());
            }
        }
    }
    tokens
}

fn expect(tokens: &[String], cursor: &mut usize, token: &str) -> Result<(), ConfigError> {
    if tokens.get(*cursor).map(String::as_str) == Some(token) {
        *cursor += 1;
        Ok(())
    } else {
        Err(ConfigError(format!(
            "expected '{token}', found '{}'",
            tokens.get(*cursor).cloned().unwrap_or_default()
        )))
    }
}

fn next_value(tokens: &[String], cursor: &mut usize, what: &str) -> Result<String, ConfigError> {
    match tokens.get(*cursor) {
        Some(token) if token != "{" && token != "}" => {
            *cursor += 1;
            Ok(token.clone())
        }
        _ => Err(ConfigError(format!("missing value for '{what}'"))),
    }
}

fn parse_server(tokens: &[String], cursor: &mut usize) -> Result<ServerConfig, ConfigError> {
    *cursor += 1; // "server"
    expect(tokens, cursor, "{")?;
    let mut server = ServerConfig::new();

    loop {
        let token = tokens
            .get(*cursor)
            .ok_or_else(|| ConfigError("unexpected end of file in server block".into()))?
            .clone();
        *cursor += 1;
        match token.as_str() {
            "}" => return Ok(server),
            "host" => server.host = next_value(tokens, cursor, "host")?,
            "port" => {
                // One or more ports.
                let mut any = false;
                while let Some(word) = tokens.get(*cursor) {
                    let Ok(port) = word.parse::<u16>() else { break };
                    if server.ports.contains(&port) {
                        return Err(ConfigError(format!(
                            "port {port} is configured twice in the same server"
                        )));
                    }
                    server.ports.push(port);
                    *cursor += 1;
                    any = true;
                }
                if !any {
                    return Err(ConfigError("port expects at least one number".into()));
                }
            }
            "server_name" => server.server_name = Some(next_value(tokens, cursor, "server_name")?),
            "error_page" => {
                let code = next_value(tokens, cursor, "error_page")?
                    .parse::<u16>()
                    .map_err(|_| ConfigError("error_page expects a status code".into()))?;
                let page = next_value(tokens, cursor, "error_page")?;
                server.error_pages.insert(code, page);
            }
            "client_max_body_size" => {
                server.max_body_size = next_value(tokens, cursor, "client_max_body_size")?
                    .parse()
                    .map_err(|_| {
                        ConfigError("client_max_body_size expects a number of bytes".into())
                    })?;
            }
            "route" => {
                let route = parse_route(tokens, cursor)?;
                server.routes.push(route);
            }
            other => {
                return Err(ConfigError(format!("unknown server directive '{other}'")));
            }
        }
    }
}

fn parse_route(tokens: &[String], cursor: &mut usize) -> Result<RouteConfig, ConfigError> {
    let prefix = next_value(tokens, cursor, "route")?;
    if !prefix.starts_with('/') {
        return Err(ConfigError(format!(
            "route prefix '{prefix}' must start with '/'"
        )));
    }
    expect(tokens, cursor, "{")?;
    let mut route = RouteConfig::new(prefix);

    loop {
        let token = tokens
            .get(*cursor)
            .ok_or_else(|| ConfigError("unexpected end of file in route block".into()))?
            .clone();
        *cursor += 1;
        match token.as_str() {
            "}" => return Ok(route),
            "root" => route.root = Some(next_value(tokens, cursor, "root")?),
            "index" => route.index = Some(next_value(tokens, cursor, "index")?),
            "redirect" => route.redirect = Some(next_value(tokens, cursor, "redirect")?),
            "methods" => {
                let mut methods = Vec::new();
                while let Some(word) = tokens.get(*cursor) {
                    let upper = word.to_uppercase();
                    if !["GET", "POST", "DELETE", "PUT", "HEAD"].contains(&upper.as_str()) {
                        break;
                    }
                    methods.push(upper);
                    *cursor += 1;
                }
                if methods.is_empty() {
                    return Err(ConfigError("methods expects at least one method".into()));
                }
                route.methods = methods;
            }
            "cgi" => {
                let extension = next_value(tokens, cursor, "cgi")?;
                let interpreter = next_value(tokens, cursor, "cgi")?;
                if !extension.starts_with('.') {
                    return Err(ConfigError("cgi extension must start with '.'".into()));
                }
                route.cgi.insert(extension, interpreter);
            }
            "directory_listing" => {
                route.directory_listing = next_value(tokens, cursor, "directory_listing")? == "on";
            }
            "upload" => {
                route.upload = next_value(tokens, cursor, "upload")? == "on";
            }
            other => {
                return Err(ConfigError(format!("unknown route directive '{other}'")));
            }
        }
    }
}

/// After a parse error, resynchronize at the next `server` block.
fn skip_block(tokens: &[String], cursor: &mut usize) {
    while *cursor < tokens.len() && tokens[*cursor] != "server" {
        *cursor += 1;
    }
}

fn validate(server: &ServerConfig) -> Result<(), ConfigError> {
    if server.ports.is_empty() {
        return Err(ConfigError("server has no port".into()));
    }
    if server.routes.is_empty() {
        return Err(ConfigError("server has no route".into()));
    }
    for route in &server.routes {
        if route.redirect.is_none() && route.root.is_none() {
            return Err(ConfigError(format!(
                "route '{}' needs either a root or a redirect",
                route.prefix
            )));
        }
    }
    Ok(())
}

/// Two servers may share a `host:port` only when a `server_name` tells them
/// apart. Repeating an address with the same name (or with no name on both
/// sides) is a configuration error: the second block could never be reached,
/// so it is reported and skipped instead of silently ignored.
fn no_conflict(accepted: &[ServerConfig], candidate: &ServerConfig) -> Result<(), ConfigError> {
    for existing in accepted {
        if !existing.host.eq_ignore_ascii_case(&candidate.host) {
            continue;
        }
        let Some(port) = candidate
            .ports
            .iter()
            .find(|port| existing.ports.contains(port))
        else {
            continue;
        };
        let same_name = match (&existing.server_name, &candidate.server_name) {
            (None, None) => true,
            (Some(existing_name), Some(new_name)) => existing_name.eq_ignore_ascii_case(new_name),
            _ => false,
        };
        if same_name {
            let name = candidate.server_name.as_deref().unwrap_or("<default>");
            return Err(ConfigError(format!(
                "{}:{port} is already taken by server_name '{name}'; a host:port \
                 may only be repeated with a different server_name",
                candidate.host
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "
        server {
            host 127.0.0.1
            port 8080 8081
            server_name example.com
            error_page 404 www/errors/404.html
            client_max_body_size 2048
            route / {
                root www/site
                index index.html
                methods GET POST
                directory_listing on
            }
            route /old {
                redirect /new
            }
        }";

    #[test]
    fn parses_a_full_server_block() {
        let (servers, errors) = parse(GOOD);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(servers.len(), 1);
        let server = &servers[0];
        assert_eq!(server.ports, vec![8080, 8081]);
        assert_eq!(server.server_name.as_deref(), Some("example.com"));
        assert_eq!(server.max_body_size, 2048);
        assert_eq!(server.routes.len(), 2);
        assert!(server.routes[0].directory_listing);
        assert_eq!(server.routes[1].redirect.as_deref(), Some("/new"));
    }

    #[test]
    fn duplicate_port_in_one_server_is_an_error() {
        let source = "server { port 8080 8080 route / { root www } }";
        let (servers, errors) = parse(source);
        assert!(servers.is_empty());
        assert!(errors[0].0.contains("configured twice"), "{errors:?}");
    }

    #[test]
    fn broken_server_does_not_kill_the_valid_one() {
        let source = format!(
            "server {{ port 9999 route / {{ bad_directive x }} }}\n{GOOD}"
        );
        let (servers, errors) = parse(&source);
        assert_eq!(servers.len(), 1, "the valid server must survive");
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn route_without_root_or_redirect_is_invalid() {
        let source = "server { port 8080 route / { methods GET } }";
        let (servers, errors) = parse(source);
        assert!(servers.is_empty());
        assert!(errors[0].0.contains("root or a redirect"));
    }

    #[test]
    fn the_same_port_twice_across_servers_is_an_error() {
        let source = "
            server { port 8080 route / { root a } }
            server { port 8080 route / { root b } }";
        let (servers, errors) = parse(source);
        assert_eq!(servers.len(), 1, "only the first server may survive");
        assert!(errors[0].0.contains("already taken"), "{errors:?}");
    }

    #[test]
    fn the_same_port_with_different_server_names_is_allowed() {
        let source = "
            server { port 8080 server_name a.com route / { root a } }
            server { port 8080 server_name b.com route / { root b } }";
        let (servers, errors) = parse(source);
        assert_eq!(servers.len(), 2, "virtual hosts may share a port");
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn head_is_allowed_wherever_get_is() {
        let (servers, _) = parse("server { port 1 route / { root a methods GET } }");
        let route = &servers[0].routes[0];
        assert!(route.allows("HEAD"));
        assert!(!route.allows("DELETE"));
    }

    #[test]
    fn longest_route_prefix_wins() {
        let (servers, _) = parse(
            "server { port 1 route / { root a } route /files { root b } }",
        );
        let server = &servers[0];
        assert_eq!(server.route_for("/files/x.txt").unwrap().root.as_deref(), Some("b"));
        assert_eq!(server.route_for("/other").unwrap().root.as_deref(), Some("a"));
        assert_eq!(server.route_for("/filesystem").unwrap().root.as_deref(), Some("a"));
    }
}
