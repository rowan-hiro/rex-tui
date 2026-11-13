use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct ResolvedServer {
    pub socket_path: PathBuf,
    pub endpoint_display: String,
    pub server_pid: Option<u32>,
    pub rex_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RexServerStatusJson {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    rex_version: Option<String>,
    #[serde(default)]
    listeners: Vec<RexListenerJson>,
}

#[derive(Debug, Deserialize)]
struct RexListenerJson {
    #[serde(default)]
    transport: String,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    addresses: Vec<String>,
}

fn parse_unix_endpoint(endpoint: &str) -> Option<PathBuf> {
    if let Some(rest) = endpoint.strip_prefix("unix://") {
        Some(PathBuf::from(rest))
    } else if endpoint.starts_with('/') {
        Some(PathBuf::from(endpoint))
    } else {
        None
    }
}

pub fn default_socket_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rt) = std::env::var("REX_RUNTIME_DIR") {
        if !rt.is_empty() {
            out.push(PathBuf::from(rt).join("server.sock"));
        }
    }
    if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        if !xdg_state.is_empty() {
            out.push(PathBuf::from(xdg_state).join("rex/run/server.sock"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            out.push(PathBuf::from(home).join(".local/state/rex/run/server.sock"));
        }
    }
    out
}

async fn can_connect_unix(path: &Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

pub async fn resolve_local_server(
    explicit_server: Option<&str>,
    autostart: bool,
) -> Result<ResolvedServer> {
    // 1. If user explicitly gave a unix socket path, check it first
    if let Some(srv) = explicit_server {
        if let Some(sock) = parse_unix_endpoint(srv) {
            if can_connect_unix(&sock).await {
                return Ok(ResolvedServer {
                    endpoint_display: format!("unix://{}", sock.display()),
                    socket_path: sock,
                    server_pid: None,
                    rex_version: None,
                });
            }
        }
    } else {
        // Check REX_SERVER env if it points to a unix socket
        if let Ok(env_srv) = std::env::var("REX_SERVER") {
            if let Some(sock) = parse_unix_endpoint(&env_srv) {
                if can_connect_unix(&sock).await {
                    return Ok(ResolvedServer {
                        endpoint_display: format!("unix://{}", sock.display()),
                        socket_path: sock,
                        server_pid: None,
                        rex_version: None,
                    });
                }
            }
        }
        // Check standard local socket candidates directly for fast startup
        for cand in default_socket_candidates() {
            if can_connect_unix(&cand).await {
                return Ok(ResolvedServer {
                    endpoint_display: format!("unix://{}", cand.display()),
                    socket_path: cand,
                    server_pid: None,
                    rex_version: None,
                });
            }
        }
    }

    // 2. Ask `rex server status --json` (which also handles autostarting the local server)
    let mut cmd = Command::new("rex");
    if let Some(srv) = explicit_server {
        cmd.arg("-S").arg(srv);
    }
    if !autostart {
        cmd.arg("--autostart=false");
    }
    cmd.args(["server", "status", "--json"]);

    let output = cmd
        .output()
        .await
        .context("failed to execute `rex server status --json` (is `rex` installed in PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "rex server unavailable: {}",
            stderr.trim()
        ));
    }

    let status: RexServerStatusJson = serde_json::from_slice(&output.stdout)
        .context("failed to parse `rex server status --json` output")?;

    if let Some(sock) = parse_unix_endpoint(&status.endpoint) {
        return Ok(ResolvedServer {
            endpoint_display: status.endpoint,
            socket_path: sock,
            server_pid: status.pid,
            rex_version: status.rex_version,
        });
    }

    for l in &status.listeners {
        if l.transport == "unix" && (l.primary || !l.addresses.is_empty()) {
            if let Some(addr) = l.addresses.first() {
                let sock = PathBuf::from(addr);
                return Ok(ResolvedServer {
                    endpoint_display: format!("unix://{}", sock.display()),
                    socket_path: sock,
                    server_pid: status.pid,
                    rex_version: status.rex_version,
                });
            }
        }
    }

    Err(anyhow!(
        "local Unix socket listener not found in rex server status (endpoint={})",
        status.endpoint
    ))
}
