use crate::discovery::ConnectTarget;
use crate::types::{
    AttachSessionResponse, CreateSessionResponse, DataTicketResponse, RexEvent, RexRpcError,
    ServerStatusResponse, ServerVersionResponse, SessionListResponse, SessionSummary, SessionView,
    TerminalBlobReadResponse, TerminalFormatResponse, WelcomeEnvelope,
};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::handshake::client::{generate_key, Request};
use tokio_tungstenite::tungstenite::Message;

pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}
pub type BoxedStream = Pin<Box<dyn IoStream>>;

async fn open_target_ws(
    target: &ConnectTarget,
    api_path: &str,
    subprotocol: &str,
) -> Result<tokio_tungstenite::WebSocketStream<BoxedStream>> {
    let (stream, host_header): (BoxedStream, String) = match target {
        ConnectTarget::Unix(path) => {
            let s = UnixStream::connect(path)
                .await
                .with_context(|| format!("无法连接 Unix Socket: {}", path.display()))?;
            (Box::pin(s), "localhost".to_string())
        }
        ConnectTarget::Tcp { addr, host_header } => {
            let s = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
                .await
                .with_context(|| format!("连接 HTTP 服务超时: {}", addr))?
                .with_context(|| format!("无法连接 HTTP 服务: {}", addr))?;
            let _ = s.set_nodelay(true);
            (Box::pin(s), host_header.clone())
        }
    };

    let uri = format!("ws://{}{}", host_header, api_path);
    let req = Request::builder()
        .uri(&uri)
        .header("Host", &host_header)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .header("Sec-WebSocket-Protocol", subprotocol)
        .body(())
        .context("构建 WebSocket 握手请求失败")?;

    let (ws, _resp) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::client_async(req, stream),
    )
    .await
    .with_context(|| format!("WebSocket 握手超时 ({})", api_path))?
    .with_context(|| format!("WebSocket 握手失败 ({})", api_path))?;

    Ok(ws)
}

#[derive(Clone)]
pub struct RexClient {
    target: ConnectTarget,
    next_id: Arc<AtomicU64>,
    outbound_tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>,
}

impl RexClient {
    pub async fn connect(
        target: ConnectTarget,
    ) -> Result<(Self, mpsc::UnboundedReceiver<RexEvent>)> {
        let mut ws = open_target_ws(&target, "/api/control", "rex.control.v1").await?;

        let hello = json!({
            "type": "hello",
            "api_revision": 13,
            "api_min_revision": 0,
            "client": {
                "kind": "app",
                "platform": std::env::consts::OS,
                "name": "rex-tui",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        ws.send(Message::Text(hello.to_string().into()))
            .await
            .context("failed to send hello on /api/control")?;

        // Read welcome message
        let welcome_env = loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
                .await
                .context("等待服务端 welcome 超时")?
                .ok_or_else(|| anyhow!("服务端在发送 welcome 前关闭了连接"))?
                .context("读取服务端 welcome 失败")?;
            match msg {
                Message::Text(txt) => {
                    let v: Value =
                        serde_json::from_str(&txt).context("invalid JSON in welcome message")?;
                    if v.get("type").and_then(|t| t.as_str()) == Some("welcome") {
                        let env: WelcomeEnvelope =
                            serde_json::from_value(v).context("malformed welcome envelope")?;
                        break env;
                    }
                }
                Message::Close(frame) => {
                    let reason = frame
                        .map(|f| format!("{} ({})", f.reason, f.code))
                        .unwrap_or_else(|| "closed".to_string());
                    return Err(anyhow!("服务端拒绝连接: {}", reason));
                }
                _ => {}
            }
        };

        if welcome_env.protocol != "rex.control.v1" {
            return Err(anyhow!(
                "unexpected control protocol: {}",
                welcome_env.protocol
            ));
        }

        let (mut ws_sink, mut ws_stream) = ws.split();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<String>();
        let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<Message>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<RexEvent>();
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = Arc::clone(&pending);

        // Writer task: exits and closes the WebSocket as soon as all `RexClient` handles drop
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(ctrl_msg) = pong_rx.recv() => {
                        if ws_sink.send(ctrl_msg).await.is_err() {
                            break;
                        }
                    }
                    maybe_text = outbound_rx.recv() => {
                        match maybe_text {
                            Some(text) => {
                                if ws_sink.send(Message::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                            None => {
                                let _ = ws_sink.close().await;
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Reader task
        tokio::spawn(async move {
            let disconnect_reason = loop {
                match ws_stream.next().await {
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                            match v.get("type").and_then(|t| t.as_str()) {
                                Some("response") => {
                                    if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                                        if let Some(tx) = pending_reader.lock().await.remove(id) {
                                            if let Some(err_val) = v.get("error") {
                                                let rpc_err: RexRpcError = serde_json::from_value(
                                                    err_val.clone(),
                                                )
                                                .unwrap_or(RexRpcError {
                                                    code: "unknown".into(),
                                                    message: err_val.to_string(),
                                                    status: 500,
                                                });
                                                let _ = tx.send(Err(anyhow!(rpc_err)));
                                            } else {
                                                let payload = v
                                                    .get("payload")
                                                    .cloned()
                                                    .unwrap_or(Value::Object(Default::default()));
                                                let _ = tx.send(Ok(payload));
                                            }
                                        }
                                    }
                                }
                                Some("event") => {
                                    if let Some(payload) = v.get("payload") {
                                        if let Some(ev) = RexEvent::from_wire(payload) {
                                            let _ = event_tx.send(ev);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = pong_tx.send(Message::Pong(data));
                    }
                    Some(Ok(Message::Close(frame))) => {
                        break frame
                            .map(|f| format!("{}", f.reason))
                            .unwrap_or_else(|| "server closed connection".into());
                    }
                    Some(Err(e)) => {
                        break e.to_string();
                    }
                    None => {
                        break "EOF from server".to_string();
                    }
                    _ => {}
                }
            };

            let mut map = pending_reader.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(anyhow!("connection closed: {}", disconnect_reason)));
            }
            let _ = event_tx.send(RexEvent::Disconnected(disconnect_reason));
        });

        let client = Self {
            target,
            next_id: Arc::new(AtomicU64::new(1)),
            outbound_tx,
            pending,
        };

        Ok((client, event_rx))
    }

    pub fn target(&self) -> &ConnectTarget {
        &self.target
    }

    pub async fn call_raw(&self, method: &str, payload: Value) -> Result<Value> {
        let id = format!("req-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        let req = json!({
            "type": "request",
            "id": id,
            "method": method,
            "payload": payload,
        });

        if self.outbound_tx.send(req.to_string()).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(anyhow!("control connection is closed"));
        }

        rx.await
            .map_err(|_| anyhow!("request canceled before response arrived"))?
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, payload: Value) -> Result<T> {
        let val = self.call_raw(method, payload).await?;
        serde_json::from_value(val)
            .with_context(|| format!("failed to decode response for {}", method))
    }

    pub async fn fetch_server_metadata(
        &self,
    ) -> (Option<u32>, Option<String>, Option<String>) {
        let status_fut = self.call::<ServerStatusResponse>("server.status", json!({}));
        let ver_fut = self.call::<ServerVersionResponse>("server.version", json!({}));
        let (st, ver) = tokio::join!(status_fut, ver_fut);
        let pid = st.ok().and_then(|s| s.pid);
        let (rex_ver, platform) = match ver {
            Ok(v) => (
                (!v.rex_version.as_deref().unwrap_or("").is_empty())
                    .then_some(v.rex_version)
                    .flatten(),
                (!v.platform.as_deref().unwrap_or("").is_empty())
                    .then_some(v.platform)
                    .flatten(),
            ),
            Err(_) => (None, None),
        };
        (pid, rex_ver, platform)
    }

    pub async fn attach_all(&self) -> Result<Vec<SessionSummary>> {
        let resp: SessionListResponse = self
            .call("session.attach_all", json!({ "event_scope": "all" }))
            .await?;
        Ok(resp.sessions)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let resp: SessionListResponse = self.call("session.list", json!({})).await?;
        Ok(resp.sessions)
    }

    pub async fn create_session(&self, label: Option<&str>) -> Result<CreateSessionResponse> {
        let mut payload = json!({
            "initial_windows": [
                {
                    "layout": {
                        "block": {
                            "flavor": "com.superlogical.terminal.shell"
                        }
                    }
                }
            ]
        });
        if let Some(lbl) = label {
            let trimmed = lbl.trim();
            if !trimmed.is_empty() {
                payload["label"] = Value::String(trimmed.to_string());
            }
        }
        self.call("session.create", payload).await
    }

    pub async fn rename_session(&self, session_id: &str, label: &str) -> Result<()> {
        let _ = self
            .call_raw(
                "session.set_label",
                json!({
                    "session_id": session_id,
                    "label": label,
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn destroy_session(&self, session_id: &str) -> Result<()> {
        let _ = self
            .call_raw("session.destroy", json!({ "session_id": session_id }))
            .await?;
        Ok(())
    }

    pub async fn attach_session(&self, session_id: &str) -> Result<SessionView> {
        let resp: AttachSessionResponse = self
            .call("session.attach", json!({ "session_id": session_id }))
            .await?;
        Ok(resp.view)
    }

    pub async fn detach_session(&self, session_id: &str) -> Result<()> {
        let _ = self
            .call_raw("session.detach", json!({ "session_id": session_id }))
            .await?;
        Ok(())
    }

    pub async fn session_view(&self, session_id: &str) -> Result<SessionView> {
        self.call("session.view", json!({ "session_id": session_id }))
            .await
    }

    pub async fn own_blocks(&self, session_id: &str, block_ids: &[String]) -> Result<()> {
        if block_ids.is_empty() {
            return Ok(());
        }
        let _ = self
            .call_raw(
                "session.own_block",
                json!({
                    "session_id": session_id,
                    "block_ids": block_ids,
                    "mode": "take",
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn focus_window(&self, session_id: &str, window_id: &str) -> Result<()> {
        let _ = self
            .call_raw(
                "session.focus_window",
                json!({
                    "session_id": session_id,
                    "window_id": window_id,
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn focus_next_window(&self, session_id: &str) -> Result<()> {
        let _ = self
            .call_raw(
                "session.focus_next_window",
                json!({ "session_id": session_id, "wrap": true }),
            )
            .await?;
        Ok(())
    }

    pub async fn focus_previous_window(&self, session_id: &str) -> Result<()> {
        let _ = self
            .call_raw(
                "session.focus_previous_window",
                json!({ "session_id": session_id, "wrap": true }),
            )
            .await?;
        Ok(())
    }

    pub async fn focus_block(&self, session_id: &str, block_id: &str) -> Result<()> {
        let _ = self
            .call_raw(
                "session.focus_block",
                json!({
                    "session_id": session_id,
                    "block_id": block_id,
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn focus_direction(&self, session_id: &str, direction: &str) -> Result<()> {
        let _ = self
            .call_raw(
                "session.focus_direction",
                json!({
                    "session_id": session_id,
                    "direction": direction,
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn focus_next_pane(&self, session_id: &str) -> Result<()> {
        let _ = self
            .call_raw(
                "session.focus_next_pane",
                json!({ "session_id": session_id, "wrap": true }),
            )
            .await?;
        Ok(())
    }

    pub async fn focus_previous_pane(&self, session_id: &str) -> Result<()> {
        let _ = self
            .call_raw(
                "session.focus_previous_pane",
                json!({ "session_id": session_id, "wrap": true }),
            )
            .await?;
        Ok(())
    }

    pub async fn toggle_zoom_block(
        &self,
        session_id: &str,
        block_id: &str,
        zoomed: bool,
    ) -> Result<()> {
        let _ = self
            .call_raw(
                "session.zoom_block",
                json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "zoom": !zoomed,
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn terminal_resize(
        &self,
        session_id: &str,
        block_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        let _ = self
            .call_raw(
                "com.superlogical.terminal.resize",
                json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "args": {
                        "columns": cols,
                        "rows": rows,
                    }
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn terminal_write(&self, session_id: &str, block_id: &str, data: &str) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let _ = self
            .call_raw(
                "com.superlogical.terminal.write",
                json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "args": {
                        "data": data,
                    }
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn terminal_format_vt(&self, session_id: &str, block_id: &str) -> Result<String> {
        let resp: TerminalFormatResponse = self
            .call(
                "com.superlogical.terminal.format",
                json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "args": {
                        "format": "vt",
                        "extra_cursor": true,
                        "extra_modes": true,
                        "extra_style": true,
                        "extra_scrolling_region": true,
                        "extra_charsets": true,
                        "max_bytes": 524288,
                    }
                }),
            )
            .await?;

        let Some(blob_id) = resp.id else {
            return Ok(resp.content);
        };

        let total = resp.total.unwrap_or(resp.content.len());
        let tail_window = 262_144usize;
        let mut offset = if total > tail_window {
            total - tail_window
        } else {
            resp.content.len()
        };
        let mut out = if offset == resp.content.len() {
            resp.content
        } else {
            String::new()
        };

        loop {
            let chunk: TerminalBlobReadResponse = self
                .call(
                    "com.superlogical.terminal.blob_read",
                    json!({
                        "session_id": session_id,
                        "block_id": block_id,
                        "args": {
                            "id": blob_id,
                            "offset": offset,
                            "max_bytes": 262144,
                        }
                    }),
                )
                .await?;
            offset += chunk.data.len();
            out.push_str(&chunk.data);
            if chunk.eof || chunk.data.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn data_connect_terminal(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<Vec<u8>> {
        let resp: DataTicketResponse = self
            .call(
                "data.connect",
                json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "data_ref_id": "terminal",
                    "subprotocol": "rex.data.terminal.v1",
                }),
            )
            .await?;

        let raw = URL_SAFE_NO_PAD
            .decode(resp.ticket.trim_end_matches('='))
            .context("invalid base64url data ticket")?;
        if raw.len() != 16 {
            return Err(anyhow!(
                "expected 16-byte data ticket, got {} bytes",
                raw.len()
            ));
        }
        Ok(raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStreamEvent {
    InitialVt(Vec<u8>),
    Output(Vec<u8>),
    Resized { cols: u16, rows: u16 },
    ChildExited { exit_code: i32 },
    Closed,
}

/// Filters `ESC _ rex1;<nonce>;<json> ESC \` APC control sequences out of the live terminal stream,
/// first skipping the initial binary Ghostty snapshot that precedes the first `resize` APC sequence.
pub struct ApcStreamFilter {
    prefix: Vec<u8>,
    skipping_snapshot: bool,
    buf: Vec<u8>,
}

impl ApcStreamFilter {
    pub fn new(nonce: u64) -> Self {
        Self {
            prefix: format!("\x1b_rex1;{};", nonce).into_bytes(),
            skipping_snapshot: true,
            buf: Vec::with_capacity(8192),
        }
    }

    pub fn is_skipping_snapshot(&self) -> bool {
        self.skipping_snapshot
    }

    pub fn push(&mut self, chunk: &[u8]) -> (bool, Vec<TerminalStreamEvent>) {
        let mut snapshot_just_ended = false;
        let mut out = Vec::new();
        self.buf.extend_from_slice(chunk);

        if self.skipping_snapshot {
            if let Some(pos) = find_subslice(&self.buf, &self.prefix) {
                self.buf.drain(..pos);
                self.skipping_snapshot = false;
                snapshot_just_ended = true;
            } else {
                let keep = self.prefix.len().saturating_sub(1);
                if self.buf.len() > keep {
                    let drain_to = self.buf.len() - keep;
                    self.buf.drain(..drain_to);
                }
                return (false, out);
            }
        }

        loop {
            if self.buf.is_empty() {
                break;
            }
            if let Some(pos) = find_subslice(&self.buf, &self.prefix) {
                if pos > 0 {
                    let vt_bytes: Vec<u8> = self.buf.drain(..pos).collect();
                    out.push(TerminalStreamEvent::Output(vt_bytes));
                }
                let body_start = self.prefix.len();
                if let Some(st_rel) = find_subslice(&self.buf[body_start..], b"\x1b\\") {
                    let json_end = body_start + st_rel;
                    if let Ok(v) = serde_json::from_slice::<Value>(&self.buf[body_start..json_end])
                    {
                        match v.get("type").and_then(|t| t.as_str()) {
                            Some("resize") => {
                                let cols =
                                    v.get("cols").and_then(|c| c.as_u64()).unwrap_or(80) as u16;
                                let rows =
                                    v.get("rows").and_then(|r| r.as_u64()).unwrap_or(24) as u16;
                                out.push(TerminalStreamEvent::Resized { cols, rows });
                            }
                            Some("child_exited") => {
                                let code =
                                    v.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;
                                out.push(TerminalStreamEvent::ChildExited { exit_code: code });
                            }
                            _ => {}
                        }
                    }
                    self.buf.drain(..json_end + 2);
                } else {
                    if self.buf.len() > 65536 {
                        let dumped: Vec<u8> = self.buf.drain(..).collect();
                        out.push(TerminalStreamEvent::Output(dumped));
                    }
                    break;
                }
            } else {
                let partial = trailing_prefix_overlap(&self.buf, &self.prefix);
                let emit_len = self.buf.len() - partial;
                if emit_len > 0 {
                    let vt_bytes: Vec<u8> = self.buf.drain(..emit_len).collect();
                    out.push(TerminalStreamEvent::Output(vt_bytes));
                }
                break;
            }
        }

        (snapshot_just_ended, out)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn trailing_prefix_overlap(buf: &[u8], prefix: &[u8]) -> usize {
    let max_k = buf.len().min(prefix.len().saturating_sub(1));
    for k in (1..=max_k).rev() {
        if buf[buf.len() - k..] == prefix[..k] {
            return k;
        }
    }
    0
}

pub async fn run_terminal_stream(
    client: RexClient,
    session_id: String,
    block_id: String,
    tx: mpsc::UnboundedSender<(String, TerminalStreamEvent)>,
) -> Result<()> {
    let ticket = client.data_connect_terminal(&session_id, &block_id).await?;
    let mut ws = open_target_ws(client.target(), "/api/data", "rex.data.terminal.v1").await?;

    // Send 17-byte HELLO: 0x01 + 16 ticket bytes
    let mut hello = Vec::with_capacity(17);
    hello.push(0x01);
    hello.extend_from_slice(&ticket);
    ws.send(Message::Binary(hello.into()))
        .await
        .context("failed to send data HELLO")?;

    let nonce = loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .context("data stream WELCOME timed out")?
            .ok_or_else(|| anyhow!("data stream closed before WELCOME"))?
            .context("error reading data WELCOME")?;
        match msg {
            Message::Binary(b) => {
                if b.first() == Some(&0x82) {
                    let reason = b.get(1).copied().unwrap_or(0xff);
                    return Err(anyhow!("data stream rejected with reason 0x{:02x}", reason));
                }
                if b.len() != 32 || b[0] != 0x81 {
                    return Err(anyhow!("unexpected data handshake message (len={})", b.len()));
                }
                let mut nonce_bytes = [0u8; 8];
                nonce_bytes.copy_from_slice(&b[24..32]);
                break u64::from_le_bytes(nonce_bytes);
            }
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            _ => {}
        }
    };

    let mut filter = ApcStreamFilter::new(nonce);
    let mut early_exit: Option<i32> = None;

    // 1. Consume the initial Ghostty binary snapshot up to the first `resize` APC marker
    while filter.is_skipping_snapshot() {
        match ws.next().await {
            Some(Ok(Message::Binary(chunk))) => {
                let (_ended, events) = filter.push(&chunk);
                for ev in events {
                    if let TerminalStreamEvent::ChildExited { exit_code } = ev {
                        early_exit = Some(exit_code);
                    }
                }
            }
            Some(Ok(Message::Ping(p))) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                let _ = tx.send((block_id, TerminalStreamEvent::Closed));
                return Ok(());
            }
            _ => {}
        }
    }

    // 2. Seed `vt100::Parser` with full screen + cursor + styles + modes via `terminal.format(vt)`
    if let Ok(vt_str) = client.terminal_format_vt(&session_id, &block_id).await {
        if tx
            .send((
                block_id.clone(),
                TerminalStreamEvent::InitialVt(vt_str.into_bytes()),
            ))
            .is_err()
        {
            let _ = ws.close(None).await;
            return Ok(());
        }
    }
    if let Some(code) = early_exit {
        let _ = tx.send((
            block_id.clone(),
            TerminalStreamEvent::ChildExited { exit_code: code },
        ));
    }

    // 3. Stream all live VT output and APC lifecycle events
    while let Some(msg_res) = ws.next().await {
        match msg_res {
            Ok(Message::Binary(chunk)) => {
                let (_snap_ended, events) = filter.push(&chunk);
                for ev in events {
                    if tx.send((block_id.clone(), ev)).is_err() {
                        let _ = ws.close(None).await;
                        return Ok(());
                    }
                }
            }
            Ok(Message::Ping(p)) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    let _ = tx.send((block_id, TerminalStreamEvent::Closed));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apc_stream_filter_skips_snapshot_and_parses_apc() {
        let nonce = 123456789u64;
        let mut filter = ApcStreamFilter::new(nonce);

        let part1 = b"\x00\xffGHOSTTY_SNAPSHOT_BYTES\x1b_rex1;";
        let part2 = b"123456789;{\"type\":\"resize\",\"cols\":100,\"rows\":30}\x1b\\hello ";
        let part3 = b"\x1b[32mworld\x1b[0m\x1b_rex1;123456789;{\"type\":\"child_exited\",\"exit_code\":42}\x1b\\!";

        let (ended1, ev1) = filter.push(part1);
        assert!(!ended1);
        assert!(ev1.is_empty());

        let (ended2, ev2) = filter.push(part2);
        assert!(ended2);
        assert_eq!(
            ev2,
            vec![
                TerminalStreamEvent::Resized { cols: 100, rows: 30 },
                TerminalStreamEvent::Output(b"hello ".to_vec()),
            ]
        );

        let (ended3, ev3) = filter.push(part3);
        assert!(!ended3);
        assert_eq!(
            ev3,
            vec![
                TerminalStreamEvent::Output(b"\x1b[32mworld\x1b[0m".to_vec()),
                TerminalStreamEvent::ChildExited { exit_code: 42 },
                TerminalStreamEvent::Output(b"!".to_vec()),
            ]
        );
    }
}
