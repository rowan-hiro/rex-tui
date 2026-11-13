use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WelcomeClient {
    pub id: String,
    #[serde(default)]
    pub reconnect_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WelcomeEnvelope {
    pub protocol: String,
    pub client: WelcomeClient,
    #[serde(default)]
    pub api_revision: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RexRpcError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub status: u16,
}

impl std::fmt::Display for RexRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RexRpcError {}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionListResponse {
    #[serde(default)]
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateSessionWindowResult {
    pub window_id: String,
    #[serde(default)]
    pub block_ids: Vec<String>,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    #[serde(default)]
    pub initial_windows: Vec<CreateSessionWindowResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttachSessionResponse {
    pub client_id: String,
    pub view: SessionView,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionView {
    pub revision: u64,
    pub session_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub active_window_id: Option<String>,
    #[serde(default)]
    pub windows: Vec<WindowView>,
}

impl SessionView {
    pub fn active_window(&self) -> Option<&WindowView> {
        if let Some(ref active_id) = self.active_window_id {
            self.windows
                .iter()
                .find(|w| w.window_id == *active_id)
                .or_else(|| self.windows.first())
        } else {
            self.windows.first()
        }
    }

    pub fn total_blocks(&self) -> usize {
        self.windows
            .iter()
            .flat_map(|w| w.layers.iter())
            .map(|l| l.blocks.len())
            .sum()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WindowView {
    pub window_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused_block_id: Option<String>,
    #[serde(default)]
    pub zoomed_block_id: Option<String>,
    #[serde(default)]
    pub layers: Vec<LayerView>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LayerView {
    pub layer_id: String,
    pub kind: String,
    pub bounds: [f64; 4],
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub layout: Option<LayoutNode>,
    #[serde(default)]
    pub blocks: Vec<BlockSummary>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LayoutNode {
    pub rect: [f64; 4],
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub split: Option<Box<SplitNode>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SplitNode {
    pub split_id: String,
    pub direction: String,
    pub ratio: f64,
    pub before: LayoutNode,
    pub after: LayoutNode,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockSummary {
    pub block_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub creator_name: String,
    #[serde(default)]
    pub flavor: String,
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DataTicketResponse {
    pub ticket: String,
    pub expires_in_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TerminalFormatResponse {
    pub content: String,
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub total: Option<usize>,
    #[serde(default)]
    pub eof: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TerminalBlobReadResponse {
    pub id: u64,
    pub data: String,
    pub total: usize,
    pub eof: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerStatusResponse {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default)]
    pub session_count: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerVersionResponse {
    #[serde(default)]
    pub rex_version: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
}

#[derive(Debug, Clone)]
pub enum RexEvent {
    SessionCreated(SessionSummary),
    SessionDestroyed { session_id: String },
    SessionsChanged,
    HostsChanged,
    SessionLabelChanged { session_id: String, label: String },
    SessionViewChanged { session_id: String, revision: u64 },
    BlockClosed { session_id: String, block_id: String },
    BlockProcessChanged {
        session_id: String,
        block_id: String,
        foreground_name: Option<String>,
    },
    BlockTitleChanged {
        session_id: String,
        block_id: String,
        title: String,
    },
    Disconnected(String),
}

impl RexEvent {
    pub fn from_wire(event_obj: &Value) -> Option<Self> {
        let ev_type = event_obj.get("type")?.as_str()?;
        let payload = event_obj.get("payload").unwrap_or(&Value::Null);
        match ev_type {
            "session_created" => {
                let s = payload.get("session")?;
                let summary: SessionSummary = serde_json::from_value(s.clone()).ok()?;
                Some(RexEvent::SessionCreated(summary))
            }
            "session_destroyed" => {
                let sid = payload.get("session_id")?.as_str()?.to_string();
                Some(RexEvent::SessionDestroyed { session_id: sid })
            }
            "session_moved" | "sessions_reordered" | "session_label_changed" | "session_updated" => {
                Some(RexEvent::SessionsChanged)
            }
            "hosts_changed" => Some(RexEvent::HostsChanged),
            "session_event" => {
                let sid = payload.get("session_id")?.as_str()?.to_string();
                let inner = payload.get("event")?;
                let inner_type = inner.get("type")?.as_str()?;
                let inner_payload = inner.get("payload").unwrap_or(&Value::Null);
                match inner_type {
                    "session_label_changed" => {
                        let label = inner_payload
                            .get("label")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some(RexEvent::SessionLabelChanged {
                            session_id: sid,
                            label,
                        })
                    }
                    "session_view_changed" => {
                        let rev = inner_payload
                            .get("revision")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        Some(RexEvent::SessionViewChanged {
                            session_id: sid,
                            revision: rev,
                        })
                    }
                    "block_closed" => {
                        let bid = inner_payload.get("block_id")?.as_str()?.to_string();
                        Some(RexEvent::BlockClosed {
                            session_id: sid,
                            block_id: bid,
                        })
                    }
                    "block_event" => {
                        let bid = inner_payload.get("block_id")?.as_str()?.to_string();
                        let name = inner_payload.get("name")?.as_str()?;
                        let bpayload = inner_payload.get("payload").unwrap_or(&Value::Null);
                        match name {
                            "process_changed" => {
                                let fg_name = bpayload
                                    .get("foreground")
                                    .and_then(|fg| fg.get("name"))
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.to_string());
                                Some(RexEvent::BlockProcessChanged {
                                    session_id: sid,
                                    block_id: bid,
                                    foreground_name: fg_name,
                                })
                            }
                            "title_changed" => {
                                let title = bpayload
                                    .get("title")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                Some(RexEvent::BlockTitleChanged {
                                    session_id: sid,
                                    block_id: bid,
                                    title,
                                })
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}
