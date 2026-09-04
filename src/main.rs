mod cgi;
mod config;
mod handlers;
mod http;
mod server;

use std::process::exit;

fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/default.conf".to_string());

    let source = match std::fs::read_to_string(&config_path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("localhost: cannot read {config_path}: {error}");
            exit(1);
        }
    };

    let (servers, errors) = config::parse(&source);
    for error in &errors {
        eprintln!("localhost: config error (server skipped): {error}");
    }
    if servers.is_empty() {
        eprintln!("localhost: no valid server in {config_path}");
        exit(1);
    }

    for server in &servers {
        eprintln!(
            "localhost: server '{}' on {}:{:?}",
            server.server_name.as_deref().unwrap_or("-"),
            server.host,
            server.ports
        );
    }

    if let Err(error) = server::run(servers) {
        eprintln!("localhost: fatal: {error}");
        exit(1);
    }
}
