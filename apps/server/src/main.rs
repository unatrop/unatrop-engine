use std::net::SocketAddr;

use uno_daemon_protocol::DEFAULT_DAEMON_ADDR;

fn resolve_auth_token(addr: SocketAddr) -> Option<String> {
    if let Ok(raw) = std::env::var("UNATROP_DAEMON_TOKEN") {
        let trimmed = raw.trim().to_owned();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if addr.ip().is_loopback()
        && std::env::var("UNATROP_DAEMON_ALLOW_INSECURE_NO_AUTH")
            .ok()
            .as_deref()
            == Some("1")
    {
        return None;
    }
    uno_daemon::load_or_create_local_daemon_token()
}

fn main() {
    let addr: SocketAddr = std::env::var("UNATROP_DAEMON_ADDR")
        .unwrap_or_else(|_| DEFAULT_DAEMON_ADDR.to_owned())
        .parse()
        .expect("invalid UNATROP_DAEMON_ADDR");
    let token = resolve_auth_token(addr);
    let allow_insecure = addr.ip().is_loopback()
        && std::env::var("UNATROP_DAEMON_ALLOW_INSECURE_NO_AUTH")
            .ok()
            .as_deref()
            == Some("1");
    if token.is_none() && !allow_insecure {
        eprintln!(
            "unatropd error: UNATROP_DAEMON_TOKEN is not set and local token file could not be initialized"
        );
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("unatropd")
        .enable_all()
        .build()
        .expect("failed to build unatropd tokio runtime");

    rt.block_on(async move {
        if let Err(err) = uno_daemon::serve(addr, token).await {
            eprintln!("unatropd error: {err}");
            std::process::exit(1);
        }
    });
}
