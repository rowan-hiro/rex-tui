use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

#[cfg(unix)]
static TUNNEL_SEQ: AtomicU64 = AtomicU64::new(1);

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportKind {
    Local,
    Unix,
    Pipe,
    Http,
    Ssh,
}

impl TransportKind {
    pub fn badge(&self) -> &'static str {
        match self {
            TransportKind::Local => "LOCAL",
            TransportKind::Unix => "UNIX",
            TransportKind::Pipe => "PIPE",
            TransportKind::Http => "HTTP",
            TransportKind::Ssh => "SSH",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConnectTarget {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    NamedPipe(String),
    Tcp { addr: String, host_header: String },
}

pub struct SshTunnelGuard {
    child: Mutex<Option<Child>>,
    local_socket: Option<PathBuf>,
}

impl std::fmt::Debug for SshTunnelGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTunnelGuard")
            .field("local_socket", &self.local_socket)
            .finish()
    }
}

impl Drop for SshTunnelGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.start_kill();
            }
        }
        if let Some(ref sock) = self.local_socket {
            let _ = std::fs::remove_file(sock);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedServer {
    pub host_label: String,
    pub transport_kind: TransportKind,
    pub endpoint_display: String,
    pub target: ConnectTarget,
    pub server_pid: Option<u32>,
    pub rex_version: Option<String>,
    pub platform: Option<String>,
    pub _ssh_tunnel: Option<Arc<SshTunnelGuard>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostItem {
    pub label: String,
    pub endpoints: Vec<String>,
    pub is_local: bool,
    pub is_adhoc: bool,
}

impl HostItem {
    pub fn local() -> Self {
        #[cfg(unix)]
        let ep_desc = "本机 Unix Socket";
        #[cfg(windows)]
        let ep_desc = "本机 Named Pipe";
        Self {
            label: "local".to_string(),
            endpoints: vec![ep_desc.to_string()],
            is_local: true,
            is_adhoc: false,
        }
    }

    pub fn protocol_hint(&self) -> &'static str {
        if self.is_local {
            return "local";
        }
        let first = self.endpoints.first().map(|s| s.as_str()).unwrap_or("");
        if first.starts_with("ssh://") {
            "ssh"
        } else if first.starts_with("http://") || first.starts_with("ws://") {
            "http"
        } else if parse_named_pipe_endpoint(first).is_some() {
            "pipe"
        } else if first.starts_with("unix://") || first.starts_with('/') {
            "unix"
        } else {
            "remote"
        }
    }
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

#[derive(Debug, Deserialize)]
struct RexHostsListJson {
    #[serde(default)]
    hosts: Vec<RexHostEntryWrapper>,
}

#[derive(Debug, Deserialize)]
struct RexHostEntryWrapper {
    host: RexHostRecord,
}

#[derive(Debug, Deserialize)]
struct RexHostRecord {
    label: String,
    #[serde(default)]
    endpoints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHttpEndpoint {
    pub addr: String,
    pub host_header: String,
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSshEndpoint {
    pub destination: String,
    pub port: Option<u16>,
    pub display: String,
    pub short_host: String,
}

pub fn parse_unix_endpoint(endpoint: &str) -> Option<PathBuf> {
    if let Some(rest) = endpoint.strip_prefix("unix://") {
        Some(PathBuf::from(rest))
    } else if endpoint.starts_with('/') && !endpoint.starts_with("//./pipe/") {
        Some(PathBuf::from(endpoint))
    } else {
        None
    }
}

pub fn parse_named_pipe_endpoint(endpoint: &str) -> Option<String> {
    let trimmed = endpoint.trim();
    let rest = trimmed.strip_prefix("npipe://").unwrap_or(trimmed);
    if let Some(name) = rest.strip_prefix(r"\\.\pipe\") {
        if !name.is_empty() {
            return Some(format!(r"\\.\pipe\{}", name));
        }
    }
    if let Some(name) = rest.strip_prefix("//./pipe/") {
        if !name.is_empty() {
            return Some(format!(r"\\.\pipe\{}", name.replace('/', r"\")));
        }
    }
    None
}

pub fn parse_http_endpoint(raw: &str) -> Result<Option<ParsedHttpEndpoint>> {
    let trimmed = raw.trim();
    if trimmed.starts_with("https://") || trimmed.starts_with("wss://") {
        return Err(anyhow!(
            "当前版本仅支持明文 http:// 与 ssh:// 连接，暂不支持 https:// ({})",
            trimmed
        ));
    }

    let rest = if let Some(r) = trimmed.strip_prefix("http://") {
        Some(r)
    } else if let Some(r) = trimmed.strip_prefix("ws://") {
        Some(r)
    } else if !trimmed.contains("://") && !trimmed.starts_with('/') && has_explicit_port(trimmed) {
        // Rex convention: "a value with a port is always an address"
        Some(trimmed)
    } else {
        None
    };

    let Some(authority_raw) = rest else {
        return Ok(None);
    };

    let authority = authority_raw.trim_end_matches('/');
    if authority.is_empty() || authority.contains('/') || authority.contains('@') {
        return Err(anyhow!(
            "无效的 HTTP 服务地址 \"{}\"（示例: http://10.0.0.5:7500）",
            trimmed
        ));
    }

    let (host_header, addr) = split_host_and_default_port(authority, 80)?;
    Ok(Some(ParsedHttpEndpoint {
        display: format!("http://{}", host_header),
        addr,
        host_header,
    }))
}

fn has_explicit_port(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((_, after)) = rest.split_once(']') {
            return after
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok())
                .is_some();
        }
        false
    } else if let Some((host, port_str)) = s.rsplit_once(':') {
        !host.is_empty() && !host.contains(':') && port_str.parse::<u16>().is_ok()
    } else {
        false
    }
}

fn split_host_and_default_port(authority: &str, default_port: u16) -> Result<(String, String)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (ipv6, after) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("IPv6 地址缺少右括号 ']': {}", authority))?;
        if ipv6.is_empty() {
            return Err(anyhow!("空的 IPv6 地址"));
        }
        if after.is_empty() {
            let hdr = format!("[{}]", ipv6);
            let addr = format!("[{}]:{}", ipv6, default_port);
            Ok((hdr, addr))
        } else if let Some(port_s) = after.strip_prefix(':') {
            let port: u16 = port_s
                .parse()
                .with_context(|| format!("无效的端口号: {}", port_s))?;
            let full = format!("[{}]:{}", ipv6, port);
            Ok((full.clone(), full))
        } else {
            Err(anyhow!("无效的地址后缀: {}", after))
        }
    } else if let Some((host, port_s)) = authority.rsplit_once(':') {
        if host.is_empty() || host.contains(':') {
            return Err(anyhow!("无效的主机地址（IPv6 请加方括号）: {}", authority));
        }
        let port: u16 = port_s
            .parse()
            .with_context(|| format!("无效的端口号: {}", port_s))?;
        let full = format!("{}:{}", host, port);
        Ok((full.clone(), full))
    } else {
        Ok((
            authority.to_string(),
            format!("{}:{}", authority, default_port),
        ))
    }
}

pub fn parse_ssh_endpoint(raw: &str) -> Result<Option<ParsedSshEndpoint>> {
    let trimmed = raw.trim();
    let Some(rest_raw) = trimmed.strip_prefix("ssh://") else {
        return Ok(None);
    };

    let rest = rest_raw.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') {
        return Err(anyhow!(
            "无效的 SSH 地址 \"{}\"（示例: ssh://user@host 或 ssh://user@host:2222）",
            trimmed
        ));
    }

    let (user_opt, host_port) = if let Some((u, hp)) = rest.rsplit_once('@') {
        if u.is_empty() {
            return Err(anyhow!("SSH 用户名不能为空: {}", trimmed));
        }
        validate_ssh_token(u, "SSH 用户名")?;
        (Some(u), hp)
    } else {
        (None, rest)
    };

    let (host_clean, host_url_repr, port) = if let Some(v6_rest) = host_port.strip_prefix('[') {
        let (ipv6, after) = v6_rest
            .split_once(']')
            .ok_or_else(|| anyhow!("SSH IPv6 地址缺少 ']': {}", trimmed))?;
        validate_ssh_token(ipv6, "SSH 主机名")?;
        let p = if after.is_empty() {
            None
        } else if let Some(ps) = after.strip_prefix(':') {
            Some(ps.parse::<u16>().context("无效的 SSH 端口")?)
        } else {
            return Err(anyhow!("无效的 SSH 地址后缀: {}", after));
        };
        (ipv6.to_string(), format!("[{}]", ipv6), p)
    } else if let Some((h, ps)) = host_port.rsplit_once(':') {
        validate_ssh_token(h, "SSH 主机名")?;
        let p = ps.parse::<u16>().context("无效的 SSH 端口")?;
        (h.to_string(), h.to_string(), Some(p))
    } else {
        validate_ssh_token(host_port, "SSH 主机名")?;
        (host_port.to_string(), host_port.to_string(), None)
    };

    let destination = match user_opt {
        Some(u) => format!("{}@{}", u, host_clean),
        None => host_clean.clone(),
    };
    let user_prefix = user_opt.map(|u| format!("{}@", u)).unwrap_or_default();
    let port_suffix = port.map(|p| format!(":{}", p)).unwrap_or_default();
    let display = format!("ssh://{}{}{}", user_prefix, host_url_repr, port_suffix);

    Ok(Some(ParsedSshEndpoint {
        destination,
        port,
        display,
        short_host: host_clean,
    }))
}

fn validate_ssh_token(s: &str, kind: &str) -> Result<()> {
    if s.is_empty()
        || s.starts_with('-')
        || s.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(anyhow!("非法的 {}: {:?}", kind, s));
    }
    Ok(())
}

#[cfg(unix)]
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
            let home_path = PathBuf::from(home);
            out.push(home_path.join(".local/state/rex/run/server.sock"));
            #[cfg(target_os = "macos")]
            out.push(home_path.join("Library/Application Support/rex/run/server.sock"));
        }
    }
    out
}

#[cfg(unix)]
async fn can_connect_unix(path: &Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

/// Loads `[Local, ...configured rex hosts...]`.
pub async fn load_host_items() -> Vec<HostItem> {
    let mut items = vec![HostItem::local()];
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new("rex")
            .args(["--timeout", "2s", "hosts", "list", "--json"])
            .output(),
    )
    .await;

    if let Ok(Ok(out)) = output {
        if out.status.success() {
            if let Ok(parsed) = serde_json::from_slice::<RexHostsListJson>(&out.stdout) {
                for entry in parsed.hosts {
                    if !entry.host.label.is_empty() && !entry.host.endpoints.is_empty() {
                        items.push(HostItem {
                            label: entry.host.label,
                            endpoints: entry.host.endpoints,
                            is_local: false,
                            is_adhoc: false,
                        });
                    }
                }
            }
        }
    }
    items
}

pub async fn add_rex_host(label: &str, endpoints: &[String]) -> Result<()> {
    let label = label.trim();
    if label.is_empty() {
        return Err(anyhow!("主机名称不能为空"));
    }
    if label.eq_ignore_ascii_case("local") {
        return Err(anyhow!("\"local\" 为保留名称，请使用其他主机名"));
    }
    if endpoints.is_empty() {
        return Err(anyhow!("请至少填写一个连接地址 (http://... 或 ssh://...)"));
    }
    // Validate each endpoint syntax upfront
    for ep in endpoints {
        if parse_unix_endpoint(ep).is_none()
            && parse_named_pipe_endpoint(ep).is_none()
            && parse_http_endpoint(ep)?.is_none()
            && parse_ssh_endpoint(ep)?.is_none()
        {
            return Err(anyhow!(
                "无效的主机地址 \"{}\"，请使用 http://host:port 或 ssh://user@host 格式",
                ep
            ));
        }
    }

    let mut cmd = Command::new("rex");
    cmd.args(["hosts", "add", label]);
    for ep in endpoints {
        cmd.arg(ep);
    }
    let out = cmd
        .output()
        .await
        .context("调用 `rex hosts add` 失败")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("{}", stderr.trim()));
    }
    Ok(())
}

pub async fn remove_rex_host(label: &str) -> Result<()> {
    let out = Command::new("rex")
        .args(["hosts", "remove", label])
        .output()
        .await
        .context("调用 `rex hosts remove` 失败")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("{}", stderr.trim()));
    }
    Ok(())
}

/// Resolves any server specification (`None`/`"local"`, `unix://...`, `http://...`, `ssh://...`, or a `rex hosts` label).
pub async fn resolve_server(
    spec: Option<&str>,
    autostart: bool,
    in_tui: bool,
) -> Result<ResolvedServer> {
    let trimmed = spec.map(|s| s.trim()).unwrap_or("");
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("local") {
        return resolve_local_only(autostart).await;
    }

    // 1. Direct endpoint (`unix://`, `/path`, `http://`, `ws://`, `host:port`, `ssh://`)
    if let Some(resolved) = try_resolve_single_endpoint(trimmed, None, autostart, in_tui).await? {
        return Ok(resolved);
    }

    // 2. Named host label from `rex hosts`
    let hosts = load_host_items().await;
    if let Some(found) = hosts
        .into_iter()
        .find(|h| !h.is_local && h.label.eq_ignore_ascii_case(trimmed))
    {
        return resolve_host_item(&found, autostart, in_tui).await;
    }

    Err(anyhow!(
        "无法解析目标主机 \"{}\"（支持主机别名、http://host:port、ssh://[user@]host[:port] 或 unix:///path）",
        trimmed
    ))
}

pub async fn resolve_host_item(
    host: &HostItem,
    autostart: bool,
    in_tui: bool,
) -> Result<ResolvedServer> {
    if host.is_local {
        return resolve_local_only(autostart).await;
    }

    let mut errors = Vec::new();
    for ep in &host.endpoints {
        match try_resolve_single_endpoint(ep, Some(&host.label), autostart, in_tui).await {
            Ok(Some(srv)) => {
                // Verify TCP/Unix socket is actually reachable before declaring victory so multi-endpoint fallback works!
                if verify_target_reachable(&srv.target).await.is_ok() {
                    return Ok(srv);
                } else {
                    errors.push(format!("{}: 连接不可达", ep));
                }
            }
            Ok(None) => {
                errors.push(format!("{}: 不支持的地址格式", ep));
            }
            Err(e) => {
                errors.push(format!("{}: {}", ep, e));
            }
        }
    }

    Err(anyhow!(
        "连接主机 \"{}\" 失败：{}",
        host.label,
        errors.join("；")
    ))
}

async fn verify_target_reachable(target: &ConnectTarget) -> Result<()> {
    match target {
        #[cfg(unix)]
        ConnectTarget::Unix(path) => {
            tokio::net::UnixStream::connect(path)
                .await
                .with_context(|| format!("无法连接 Unix Socket {}", path.display()))?;
            Ok(())
        }
        #[cfg(windows)]
        ConnectTarget::NamedPipe(pipe_name) => {
            let _ = crate::client::open_named_pipe(pipe_name).await?;
            Ok(())
        }
        ConnectTarget::Tcp { addr, .. } => {
            tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
                .await
                .with_context(|| format!("连接 {} 超时", addr))?
                .with_context(|| format!("无法连接 {}", addr))?;
            Ok(())
        }
    }
}

async fn try_resolve_single_endpoint(
    raw: &str,
    label_override: Option<&str>,
    autostart: bool,
    in_tui: bool,
) -> Result<Option<ResolvedServer>> {
    // A1. Windows Named Pipe path (`\\.\pipe\...` or `npipe:////./pipe/...`)
    if let Some(pipe) = parse_named_pipe_endpoint(raw) {
        #[cfg(windows)]
        {
            let _ = crate::client::open_named_pipe(&pipe).await?;
            let label = label_override
                .map(|s| s.to_string())
                .unwrap_or_else(|| "pipe".to_string());
            return Ok(Some(ResolvedServer {
                host_label: label,
                transport_kind: TransportKind::Pipe,
                endpoint_display: pipe.clone(),
                target: ConnectTarget::NamedPipe(pipe),
                server_pid: None,
                rex_version: None,
                platform: None,
                _ssh_tunnel: None,
            }));
        }
        #[cfg(not(windows))]
        {
            let _ = (pipe, label_override);
            return Err(anyhow!("当前系统不支持 Windows Named Pipe 地址: {}", raw));
        }
    }

    // A2. Unix socket path (`unix:///...` or `/path/...`)
    if let Some(sock) = parse_unix_endpoint(raw) {
        #[cfg(unix)]
        {
            if !can_connect_unix(&sock).await {
                return Err(anyhow!("Unix Socket 不可用: {}", sock.display()));
            }
            let label = label_override
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unix".to_string());
            return Ok(Some(ResolvedServer {
                host_label: label,
                transport_kind: TransportKind::Unix,
                endpoint_display: format!("unix://{}", sock.display()),
                target: ConnectTarget::Unix(sock),
                server_pid: None,
                rex_version: None,
                platform: None,
                _ssh_tunnel: None,
            }));
        }
        #[cfg(not(unix))]
        {
            let _ = sock;
            return Err(anyhow!(
                "Windows 本机不支持直连 unix:// 地址 ({})，请使用 ssh:// 或 http:// 连接远程机器",
                raw
            ));
        }
    }

    // B. HTTP / WS / host:port
    if let Some(http) = parse_http_endpoint(raw)? {
        let label = label_override
            .map(|s| s.to_string())
            .unwrap_or_else(|| http.host_header.clone());
        return Ok(Some(ResolvedServer {
            host_label: label,
            transport_kind: TransportKind::Http,
            endpoint_display: http.display,
            target: ConnectTarget::Tcp {
                addr: http.addr,
                host_header: http.host_header,
            },
            server_pid: None,
            rex_version: None,
            platform: None,
            _ssh_tunnel: None,
        }));
    }

    // C. SSH (`ssh://[user@]host[:port]`)
    if let Some(ssh) = parse_ssh_endpoint(raw)? {
        let resolved = establish_ssh_tunnel(&ssh, label_override, autostart, in_tui).await?;
        return Ok(Some(resolved));
    }

    Ok(None)
}

#[cfg(unix)]
async fn resolve_local_only(autostart: bool) -> Result<ResolvedServer> {
    if let Ok(env_srv) = std::env::var("REX_SERVER") {
        if let Some(sock) = parse_unix_endpoint(&env_srv) {
            if can_connect_unix(&sock).await {
                return Ok(ResolvedServer {
                    host_label: "local".into(),
                    transport_kind: TransportKind::Local,
                    endpoint_display: format!("unix://{}", sock.display()),
                    target: ConnectTarget::Unix(sock),
                    server_pid: None,
                    rex_version: None,
                    platform: None,
                    _ssh_tunnel: None,
                });
            }
        }
    }

    for cand in default_socket_candidates() {
        if can_connect_unix(&cand).await {
            return Ok(ResolvedServer {
                host_label: "local".into(),
                transport_kind: TransportKind::Local,
                endpoint_display: format!("unix://{}", cand.display()),
                target: ConnectTarget::Unix(cand),
                server_pid: None,
                rex_version: None,
                platform: None,
                _ssh_tunnel: None,
            });
        }
    }

    let mut cmd = Command::new("rex");
    if !autostart {
        cmd.arg("--autostart=false");
    }
    cmd.args(["server", "status", "--json"]);

    let output = cmd
        .output()
        .await
        .context("执行 `rex server status --json` 失败（请确认本机已安装 rex）")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("本地 Rex 服务不可用: {}", stderr.trim()));
    }

    let status: RexServerStatusJson = serde_json::from_slice(&output.stdout)
        .context("解析 `rex server status --json` 输出失败")?;

    let sock_str = extract_remote_unix_socket_str(&status)
        .ok_or_else(|| anyhow!("未在本地 Rex 服务状态中找到 Unix Socket ({})", status.endpoint))?;
    let sock = PathBuf::from(&sock_str);

    Ok(ResolvedServer {
        host_label: "local".into(),
        transport_kind: TransportKind::Local,
        endpoint_display: format!("unix://{}", sock.display()),
        target: ConnectTarget::Unix(sock),
        server_pid: status.pid,
        rex_version: status.rex_version,
        platform: None,
        _ssh_tunnel: None,
    })
}

#[cfg(windows)]
async fn resolve_local_only(autostart: bool) -> Result<ResolvedServer> {
    if let Ok(env_srv) = std::env::var("REX_SERVER") {
        if let Some(pipe) = parse_named_pipe_endpoint(&env_srv) {
            if crate::client::open_named_pipe(&pipe).await.is_ok() {
                return Ok(ResolvedServer {
                    host_label: "local".into(),
                    transport_kind: TransportKind::Local,
                    endpoint_display: pipe.clone(),
                    target: ConnectTarget::NamedPipe(pipe),
                    server_pid: None,
                    rex_version: None,
                    platform: None,
                    _ssh_tunnel: None,
                });
            }
        }
    }

    let mut cmd = Command::new("rex");
    if !autostart {
        cmd.arg("--autostart=false");
    }
    cmd.args(["server", "status", "--json"]);

    let output = cmd
        .output()
        .await
        .context("执行 `rex server status --json` 失败（请确认本机已安装 rex，或使用 -S 连接远程主机）")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("本地 Rex 服务不可用: {}", stderr.trim()));
    }

    let status: RexServerStatusJson = serde_json::from_slice(&output.stdout)
        .context("解析 `rex server status --json` 输出失败")?;

    let pipe = extract_named_pipe_from_status(&status).ok_or_else(|| {
        anyhow!(
            "未在本地 Rex 服务状态中找到 Windows Named Pipe ({})",
            status.endpoint
        )
    })?;

    Ok(ResolvedServer {
        host_label: "local".into(),
        transport_kind: TransportKind::Local,
        endpoint_display: pipe.clone(),
        target: ConnectTarget::NamedPipe(pipe),
        server_pid: status.pid,
        rex_version: status.rex_version,
        platform: None,
        _ssh_tunnel: None,
    })
}

fn extract_remote_unix_socket_str(status: &RexServerStatusJson) -> Option<String> {
    if let Some(rest) = status.endpoint.strip_prefix("unix://") {
        if !rest.is_empty() {
            return Some(rest.to_string());
        }
    } else if status.endpoint.starts_with('/') && !status.endpoint.starts_with("//./pipe/") {
        return Some(status.endpoint.clone());
    }
    for l in &status.listeners {
        if l.transport == "unix" && (l.primary || !l.addresses.is_empty()) {
            if let Some(addr) = l.addresses.first() {
                if !addr.is_empty() {
                    return Some(addr.clone());
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn extract_named_pipe_from_status(status: &RexServerStatusJson) -> Option<String> {
    if let Some(pipe) = parse_named_pipe_endpoint(&status.endpoint) {
        return Some(pipe);
    }
    for l in &status.listeners {
        if l.transport == "named_pipe" && (l.primary || !l.addresses.is_empty()) {
            if let Some(addr) = l.addresses.first() {
                if let Some(pipe) = parse_named_pipe_endpoint(addr) {
                    return Some(pipe);
                }
                if !addr.is_empty() {
                    return Some(addr.clone());
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn ensure_short_tunnel_dir() -> Result<PathBuf> {
    let uid = unsafe { libc_geteuid() };
    let dir = PathBuf::from(format!("/tmp/rex-tui-{}", uid));
    if !dir.exists() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("创建 SSH 隧道临时目录失败: {}", dir.display()))?;
    }
    Ok(dir)
}

#[cfg(unix)]
unsafe fn libc_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}

async fn establish_ssh_tunnel(
    ssh: &ParsedSshEndpoint,
    label_override: Option<&str>,
    autostart: bool,
    in_tui: bool,
) -> Result<ResolvedServer> {
    #[cfg(unix)]
    let tunnel_dir = ensure_short_tunnel_dir()?;

    // 1. Discover & auto-start remote Rex server via SSH, retrieving its Unix socket path
    let mut probe_cmd = Command::new("ssh");
    if let Some(port) = ssh.port {
        probe_cmd.arg("-p").arg(port.to_string());
    }
    if in_tui {
        probe_cmd.args(["-o", "BatchMode=yes"]);
    } else {
        #[cfg(unix)]
        {
            let control_path = tunnel_dir.join("cm-%C");
            probe_cmd.args([
                "-o",
                "ControlMaster=auto",
                "-o",
                &format!("ControlPath={}", control_path.display()),
                "-o",
                "ControlPersist=60s",
            ]);
        }
    }
    probe_cmd.args(["-o", "ConnectTimeout=10", "--", &ssh.destination]);

    let autostart_flag = if autostart { "" } else { " --autostart=false" };
    let remote_script = format!(
        "PATH=\"$HOME/.local/bin:/usr/local/bin:/opt/homebrew/bin:$PATH\"; export PATH; rex{} server status --json",
        autostart_flag
    );
    probe_cmd.arg(remote_script).kill_on_drop(true);

    let probe_out = tokio::time::timeout(Duration::from_secs(20), probe_cmd.output())
        .await
        .with_context(|| format!("SSH 连接 {} 超时", ssh.display))?
        .with_context(|| format!("无法启动本机 `ssh` 客户端连接 {}", ssh.display))?;

    if !probe_out.status.success() {
        let stderr = String::from_utf8_lossy(&probe_out.stderr);
        let hint = if in_tui && stderr.contains("Permission denied") {
            "（TUI 内连接需配置 SSH 免密公钥或 ssh-agent）"
        } else {
            ""
        };
        return Err(anyhow!(
            "远程探测失败 ({}): {}{}",
            ssh.display,
            stderr.trim(),
            hint
        ));
    }

    let remote_status: RexServerStatusJson = serde_json::from_slice(&probe_out.stdout)
        .with_context(|| {
            format!(
                "解析远程 {} 的 `rex server status --json` 失败（请确认远程机器已安装 rex）",
                ssh.display
            )
        })?;

    let remote_sock = extract_remote_unix_socket_str(&remote_status).ok_or_else(|| {
        anyhow!(
            "远程主机 {} 未报告可用的 Unix Socket (endpoint={})",
            ssh.display,
            remote_status.endpoint
        )
    })?;

    // 2. Spawn persistent `ssh -N -L ...` forward to the remote Unix domain socket
    #[cfg(unix)]
    let (target, guard) = {
        let seq = TUNNEL_SEQ.fetch_add(1, Ordering::Relaxed);
        let local_sock = tunnel_dir.join(format!("tun-{}-{}.sock", std::process::id(), seq));
        let _ = std::fs::remove_file(&local_sock);

        let forward_spec = format!("{}:{}", local_sock.display(), remote_sock);
        let mut tun_cmd = Command::new("ssh");
        tun_cmd.arg("-N");
        if let Some(port) = ssh.port {
            tun_cmd.arg("-p").arg(port.to_string());
        }
        if in_tui {
            tun_cmd.args(["-o", "BatchMode=yes"]);
        } else {
            let control_path = tunnel_dir.join("cm-%C");
            tun_cmd.args([
                "-o",
                "ControlMaster=auto",
                "-o",
                &format!("ControlPath={}", control_path.display()),
            ]);
        }
        tun_cmd
            .args([
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StreamLocalBindUnlink=yes",
                "-o",
                "StreamLocalBindMask=0177",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ConnectTimeout=10",
                "-L",
                &forward_spec,
                "--",
                &ssh.destination,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = tun_cmd
            .spawn()
            .with_context(|| format!("启动 SSH 隧道进程失败 ({})", ssh.display))?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        loop {
            if can_connect_unix(&local_sock).await {
                break;
            }
            if let Some(exit_status) = child.try_wait()? {
                let mut err_buf = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut err_buf).await;
                }
                let _ = std::fs::remove_file(&local_sock);
                return Err(anyhow!(
                    "SSH 隧道异常退出 ({}): {}",
                    exit_status,
                    err_buf.trim()
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                let _ = std::fs::remove_file(&local_sock);
                return Err(anyhow!("等待 SSH Unix Socket 隧道就绪超时 ({})", ssh.display));
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let guard = Arc::new(SshTunnelGuard {
            child: Mutex::new(Some(child)),
            local_socket: Some(local_sock.clone()),
        });
        (ConnectTarget::Unix(local_sock), guard)
    };

    #[cfg(windows)]
    let (target, guard) = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("无法在 127.0.0.1 上分配本地 SSH 转发端口")?;
        let local_port = listener.local_addr()?.port();
        drop(listener);

        let local_addr = format!("127.0.0.1:{}", local_port);
        let forward_spec = format!("{}:{}", local_addr, remote_sock);
        let mut tun_cmd = Command::new("ssh");
        tun_cmd.arg("-N");
        if let Some(port) = ssh.port {
            tun_cmd.arg("-p").arg(port.to_string());
        }
        if in_tui {
            tun_cmd.args(["-o", "BatchMode=yes"]);
        }
        tun_cmd
            .args([
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ConnectTimeout=10",
                "-L",
                &forward_spec,
                "--",
                &ssh.destination,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = tun_cmd
            .spawn()
            .with_context(|| format!("启动 SSH 隧道进程失败 ({})", ssh.display))?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        loop {
            if tokio::net::TcpStream::connect(&local_addr).await.is_ok() {
                break;
            }
            if let Some(exit_status) = child.try_wait()? {
                let mut err_buf = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut err_buf).await;
                }
                return Err(anyhow!(
                    "SSH 隧道异常退出 ({}): {}",
                    exit_status,
                    err_buf.trim()
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                return Err(anyhow!("等待 SSH 本地转发端口就绪超时 ({})", ssh.display));
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let guard = Arc::new(SshTunnelGuard {
            child: Mutex::new(Some(child)),
            local_socket: None,
        });
        (
            ConnectTarget::Tcp {
                addr: local_addr,
                host_header: "localhost".to_string(),
            },
            guard,
        )
    };

    let label = label_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| ssh.short_host.clone());

    Ok(ResolvedServer {
        host_label: label,
        transport_kind: TransportKind::Ssh,
        endpoint_display: ssh.display.clone(),
        target,
        server_pid: remote_status.pid,
        rex_version: remote_status.rex_version,
        platform: None,
        _ssh_tunnel: Some(guard),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_http_endpoints() {
        let p1 = parse_http_endpoint("http://10.0.0.5:7500").unwrap().unwrap();
        assert_eq!(p1.addr, "10.0.0.5:7500");
        assert_eq!(p1.host_header, "10.0.0.5:7500");
        assert_eq!(p1.display, "http://10.0.0.5:7500");

        let p2 = parse_http_endpoint("http://studio.tail1234.ts.net/")
            .unwrap()
            .unwrap();
        assert_eq!(p2.addr, "studio.tail1234.ts.net:80");
        assert_eq!(p2.host_header, "studio.tail1234.ts.net");

        // Bare host:port is treated as HTTP
        let p3 = parse_http_endpoint("127.0.0.1:18921").unwrap().unwrap();
        assert_eq!(p3.addr, "127.0.0.1:18921");

        // Plain label without port returns Ok(None)
        assert!(parse_http_endpoint("devbox").unwrap().is_none());

        // https:// returns explicit error
        assert!(parse_http_endpoint("https://example.com").is_err());
    }

    #[test]
    fn test_parse_ssh_endpoints() {
        let s1 = parse_ssh_endpoint("ssh://alice@devbox:2222").unwrap().unwrap();
        assert_eq!(s1.destination, "alice@devbox");
        assert_eq!(s1.port, Some(2222));
        assert_eq!(s1.display, "ssh://alice@devbox:2222");
        assert_eq!(s1.short_host, "devbox");

        let s2 = parse_ssh_endpoint("ssh://root@[::1]:2200").unwrap().unwrap();
        assert_eq!(s2.destination, "root@::1");
        assert_eq!(s2.port, Some(2200));
        assert_eq!(s2.display, "ssh://root@[::1]:2200");

        // Reject option injection
        assert!(parse_ssh_endpoint("ssh://-oProxyCommand=evil").is_err());
    }

    #[test]
    fn test_parse_named_pipe_endpoints() {
        assert_eq!(
            parse_named_pipe_endpoint(r"\\.\pipe\rex-default"),
            Some(r"\\.\pipe\rex-default".to_string())
        );
        assert_eq!(
            parse_named_pipe_endpoint("npipe:////./pipe/rex/server"),
            Some(r"\\.\pipe\rex\server".to_string())
        );
        assert_eq!(parse_named_pipe_endpoint("/tmp/rex.sock"), None);
        assert_eq!(parse_unix_endpoint("//./pipe/rex"), None);
    }
}
