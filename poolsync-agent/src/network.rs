use anyhow::{Context, Result};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::sleep;
use tracing::warn;

const TCP_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const HUB_WAIT_INITIAL: Duration = Duration::from_secs(2);
const HUB_WAIT_MAX: Duration = Duration::from_secs(10);

/// Parse `ws://192.0.2.1:9470/ws` → (`192.0.2.1`, 9470).
pub fn hub_tcp_endpoint(hub_url: &str) -> Result<(String, u16)> {
    let url = hub_url.trim();
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .with_context(|| format!("hub_url invalide: {url}"))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    if let Some((host, port)) = host_port.rsplit_once(':') {
        let port: u16 = port
            .parse()
            .with_context(|| format!("port hub invalide: {port}"))?;
        return Ok((host.to_string(), port));
    }
    Ok((host_port.to_string(), 9470))
}

/// Convertit l'URL WebSocket du hub vers sa base HTTP sans perdre TLS.
pub fn hub_http_base(hub_url: &str) -> Result<String> {
    let url = hub_url.trim();
    let base = if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        anyhow::bail!("hub_url invalide: {url}");
    };
    Ok(base
        .strip_suffix("/ws")
        .unwrap_or(&base)
        .trim_end_matches('/')
        .to_string())
}

pub async fn hub_tcp_reachable(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    tokio::time::timeout(TCP_PROBE_TIMEOUT, TcpStream::connect(&addr))
        .await
        .ok()
        .and_then(|r| r.ok())
        .is_some()
}

/// Attend que le hub soit joignable (VPN / réseau revenu).
pub async fn wait_for_hub(host: &str, port: u16) {
    let mut wait = HUB_WAIT_INITIAL;
    loop {
        if hub_tcp_reachable(host, port).await {
            return;
        }
        warn!(
            "hub {host}:{port} inaccessible (VPN coupé?) — nouvel essai dans {}s",
            wait.as_secs()
        );
        sleep(wait).await;
        wait = std::cmp::min(wait * 2, HUB_WAIT_MAX);
    }
}
