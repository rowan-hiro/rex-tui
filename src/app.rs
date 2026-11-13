use crate::client::{run_terminal_stream, RexClient, TerminalStreamEvent};
use crate::discovery::{
    add_rex_host, load_host_items, remove_rex_host, resolve_host_item, resolve_server, HostItem,
    ResolvedServer, TransportKind,
};
use crate::input::{encode_key_to_vt, encode_paste_to_vt};
use crate::layout::{compute_window_panes, PanePlacement};
use crate::terminal_pane::TerminalPane;
use crate::types::{RexEvent, SessionSummary, SessionView};
use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::Rect;
use ratatui::DefaultTerminal;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub enum ManagerModal {
    None,
    NewSession {
        input: String,
        attach_immediately: bool,
    },
    RenameSession {
        session_id: String,
        input: String,
    },
    ConfirmDelete {
        session_id: String,
        label: String,
    },
    HostSwitcher {
        selected: usize,
    },
    AddHost {
        label: String,
        endpoint: String,
        focus_field: u8, // 0 = label, 1 = endpoint
        connect_now: bool,
    },
    QuickConnect {
        endpoint: String,
    },
    ConfirmDeleteHost {
        label: String,
        is_adhoc: bool,
    },
    Help,
}

pub struct ConnectingState {
    pub description: String,
    pub task: JoinHandle<()>,
}

pub struct AttachedState {
    pub session_id: String,
    pub view: SessionView,
    pub panes: HashMap<String, TerminalPane>,
    pub owned_blocks: HashSet<String>,
    pub prefix_active: bool,
    pub scroll_mode: bool,
    pub show_help: bool,
    pub tab_hitboxes: Vec<(u16, u16, String)>, // (start_x, end_x_exclusive, window_id)
}

pub enum AppMode {
    SessionManager,
    Attached(Box<AttachedState>),
}

enum InternalMsg {
    SessionViewLoaded {
        host_label: String,
        view: SessionView,
    },
    AttachedViewUpdated(SessionView),
    StatusError(String),
    StatusInfo(String),
    AttachReady(SessionView),
    HostsReloaded(Vec<HostItem>),
    HostConnected {
        server: ResolvedServer,
        client: RexClient,
        event_rx: mpsc::UnboundedReceiver<RexEvent>,
        sessions: Vec<SessionSummary>,
        adhoc_item: Option<HostItem>,
    },
    HostConnectFailed(String),
}

pub struct App {
    pub server: ResolvedServer,
    pub client: RexClient,
    pub autostart: bool,
    pub hosts: Vec<HostItem>,
    pub host_hitboxes: Vec<(u16, u16, usize)>, // (x0, x1, host_index) in row 3
    pub connecting: Option<ConnectingState>,

    pub sessions: Vec<SessionSummary>,
    pub selected_idx: usize,
    pub session_previews: HashMap<String, SessionView>,
    pub mode: AppMode,
    pub modal: ManagerModal,
    pub status_msg: Option<(String, bool)>, // (text, is_error)
    pub should_quit: bool,
    pub term_size: Rect,

    event_rx: mpsc::UnboundedReceiver<RexEvent>,
    stream_tx: mpsc::UnboundedSender<(String, TerminalStreamEvent)>,
    stream_rx: mpsc::UnboundedReceiver<(String, TerminalStreamEvent)>,
    internal_tx: mpsc::UnboundedSender<InternalMsg>,
    internal_rx: mpsc::UnboundedReceiver<InternalMsg>,
    write_tx: mpsc::UnboundedSender<(RexClient, String, String, String)>,
}

impl App {
    pub async fn new(
        mut server: ResolvedServer,
        client: RexClient,
        event_rx: mpsc::UnboundedReceiver<RexEvent>,
        initial_session: Option<&str>,
        autostart: bool,
    ) -> Result<Self> {
        if server.server_pid.is_none() || server.rex_version.is_none() {
            let (pid, ver, plat) = client.fetch_server_metadata().await;
            if server.server_pid.is_none() {
                server.server_pid = pid;
            }
            if server.rex_version.is_none() {
                server.rex_version = ver;
            }
            if server.platform.is_none() {
                server.platform = plat;
            }
        }

        let sessions = client.attach_all().await?;
        let mut hosts = load_host_items().await;
        if server.transport_kind != TransportKind::Local
            && !hosts
                .iter()
                .any(|h| h.label.eq_ignore_ascii_case(&server.host_label))
        {
            hosts.push(HostItem {
                label: server.host_label.clone(),
                endpoints: vec![server.endpoint_display.clone()],
                is_local: false,
                is_adhoc: true,
            });
        }

        let (stream_tx, stream_rx) = mpsc::unbounded_channel();
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let (write_tx, mut write_rx) =
            mpsc::unbounded_channel::<(RexClient, String, String, String)>();

        // Sequential + coalescing terminal input writer task across any active RexClient
        tokio::spawn(async move {
            let mut buffered_next: Option<(RexClient, String, String, String)> = None;
            loop {
                let (wclient, sid, bid, mut batch) = match buffered_next.take() {
                    Some(item) => item,
                    None => match write_rx.recv().await {
                        Some(item) => item,
                        None => break,
                    },
                };
                while let Ok((nc, nsid, nbid, ndata)) = write_rx.try_recv() {
                    if nsid == sid && nbid == bid && batch.len() + ndata.len() < 65536 {
                        batch.push_str(&ndata);
                    } else {
                        buffered_next = Some((nc, nsid, nbid, ndata));
                        break;
                    }
                }
                let _ = wclient.terminal_write(&sid, &bid, &batch).await;
            }
        });

        let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));

        let mut app = Self {
            server,
            client,
            autostart,
            hosts,
            host_hitboxes: Vec::new(),
            connecting: None,
            sessions,
            selected_idx: 0,
            session_previews: HashMap::new(),
            mode: AppMode::SessionManager,
            modal: ManagerModal::None,
            status_msg: None,
            should_quit: false,
            term_size: Rect::new(0, 0, cols, rows),
            event_rx,
            stream_tx,
            stream_rx,
            internal_tx,
            internal_rx,
            write_tx,
        };

        if let Some(target) = initial_session {
            if let Some(found) = app.find_session_by_query(target) {
                app.attach_to_session(&found.session_id).await?;
            } else {
                app.status_msg =
                    Some((format!("未找到会话 \"{}\"，已打开会话管理列表", target), true));
                app.request_selected_preview();
            }
        } else {
            app.request_selected_preview();
        }

        Ok(app)
    }

    pub fn active_host_index(&self) -> usize {
        if self.server.transport_kind == TransportKind::Local {
            return 0;
        }
        self.hosts
            .iter()
            .position(|h| {
                !h.is_local
                    && (h.label.eq_ignore_ascii_case(&self.server.host_label)
                        || h.endpoints
                            .iter()
                            .any(|ep| ep == &self.server.endpoint_display))
            })
            .unwrap_or(0)
    }

    fn merge_loaded_hosts(&mut self, mut new_hosts: Vec<HostItem>) {
        for existing in &self.hosts {
            if existing.is_adhoc
                && !new_hosts
                    .iter()
                    .any(|h| h.label.eq_ignore_ascii_case(&existing.label))
            {
                new_hosts.push(existing.clone());
            }
        }
        self.hosts = new_hosts;
    }

    fn reload_hosts_bg(&self) {
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let items = load_host_items().await;
            let _ = tx.send(InternalMsg::HostsReloaded(items));
        });
    }

    fn start_connect_host_item(&mut self, item: HostItem) {
        if let Some(prev) = self.connecting.take() {
            prev.task.abort();
        }
        let desc = format!(
            "{} ({})",
            item.label,
            item.endpoints.first().cloned().unwrap_or_default()
        );
        let autostart = self.autostart;
        let tx = self.internal_tx.clone();
        let item_clone = item.clone();

        let task = tokio::spawn(async move {
            let res: Result<_> = async {
                let mut srv = resolve_host_item(&item_clone, autostart, true).await?;
                let (client, event_rx) = RexClient::connect(srv.target.clone()).await?;
                if srv.server_pid.is_none() || srv.rex_version.is_none() {
                    let (pid, ver, plat) = client.fetch_server_metadata().await;
                    if srv.server_pid.is_none() {
                        srv.server_pid = pid;
                    }
                    if srv.rex_version.is_none() {
                        srv.rex_version = ver;
                    }
                    srv.platform = plat;
                }
                let sessions = client.attach_all().await?;
                let adhoc = item_clone.is_adhoc.then_some(item_clone);
                Ok((srv, client, event_rx, sessions, adhoc))
            }
            .await;

            match res {
                Ok((server, client, event_rx, sessions, adhoc_item)) => {
                    let _ = tx.send(InternalMsg::HostConnected {
                        server,
                        client,
                        event_rx,
                        sessions,
                        adhoc_item,
                    });
                }
                Err(e) => {
                    let _ = tx.send(InternalMsg::HostConnectFailed(format!("{:#}", e)));
                }
            }
        });

        self.connecting = Some(ConnectingState {
            description: desc,
            task,
        });
    }

    fn start_connect_adhoc(&mut self, endpoint: String) {
        let trimmed = endpoint.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        if let Some(prev) = self.connecting.take() {
            prev.task.abort();
        }
        let autostart = self.autostart;
        let tx = self.internal_tx.clone();
        let desc = trimmed.clone();

        let task = tokio::spawn(async move {
            let res: Result<_> = async {
                let mut srv = resolve_server(Some(&trimmed), autostart, true).await?;
                let (client, event_rx) = RexClient::connect(srv.target.clone()).await?;
                if srv.server_pid.is_none() || srv.rex_version.is_none() {
                    let (pid, ver, plat) = client.fetch_server_metadata().await;
                    if srv.server_pid.is_none() {
                        srv.server_pid = pid;
                    }
                    if srv.rex_version.is_none() {
                        srv.rex_version = ver;
                    }
                    srv.platform = plat;
                }
                let sessions = client.attach_all().await?;
                let adhoc = (srv.transport_kind != TransportKind::Local).then(|| HostItem {
                    label: srv.host_label.clone(),
                    endpoints: vec![srv.endpoint_display.clone()],
                    is_local: false,
                    is_adhoc: true,
                });
                Ok((srv, client, event_rx, sessions, adhoc))
            }
            .await;

            match res {
                Ok((server, client, event_rx, sessions, adhoc_item)) => {
                    let _ = tx.send(InternalMsg::HostConnected {
                        server,
                        client,
                        event_rx,
                        sessions,
                        adhoc_item,
                    });
                }
                Err(e) => {
                    let _ = tx.send(InternalMsg::HostConnectFailed(format!("{:#}", e)));
                }
            }
        });

        self.connecting = Some(ConnectingState {
            description: desc,
            task,
        });
    }

    fn find_session_by_query(&self, query: &str) -> Option<SessionSummary> {
        self.sessions
            .iter()
            .find(|s| s.session_id == query || s.label == query)
            .or_else(|| self.sessions.iter().find(|s| s.session_id.ends_with(query)))
            .cloned()
    }

    pub fn selected_session(&self) -> Option<&SessionSummary> {
        self.sessions.get(self.selected_idx)
    }

    fn clamp_selection(&mut self) {
        if self.sessions.is_empty() {
            self.selected_idx = 0;
        } else if self.selected_idx >= self.sessions.len() {
            self.selected_idx = self.sessions.len() - 1;
        }
    }

    fn request_selected_preview(&self) {
        let Some(sess) = self.selected_session() else {
            return;
        };
        let sid = sess.session_id.clone();
        let host_label = self.server.host_label.clone();
        let client = self.client.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            if let Ok(view) = client.session_view(&sid).await {
                let _ = tx.send(InternalMsg::SessionViewLoaded { host_label, view });
            }
        });
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut reader = EventStream::new();

        while !self.should_quit {
            if let Ok(sz) = terminal.size() {
                let new_rect = Rect::new(0, 0, sz.width, sz.height);
                if new_rect != self.term_size {
                    self.term_size = new_rect;
                    self.sync_attached_panes();
                }
            }

            terminal.draw(|frame| {
                crate::ui::render(frame, self);
            })?;

            tokio::select! {
                maybe_ev = reader.next() => {
                    match maybe_ev {
                        Some(Ok(ev)) => self.handle_crossterm_event(ev).await,
                        Some(Err(e)) => {
                            self.status_msg = Some((format!("终端输入读取错误: {}", e), true));
                        }
                        None => break,
                    }
                }
                Some(rex_ev) = self.event_rx.recv() => {
                    self.handle_rex_event(rex_ev).await;
                    while let Ok(more) = self.event_rx.try_recv() {
                        self.handle_rex_event(more).await;
                    }
                }
                Some((block_id, stream_ev)) = self.stream_rx.recv() => {
                    self.apply_block_stream_event(&block_id, stream_ev);
                    let mut budget = 256;
                    while budget > 0 {
                        match self.stream_rx.try_recv() {
                            Ok((bid, sev)) => {
                                self.apply_block_stream_event(&bid, sev);
                                budget -= 1;
                            }
                            Err(_) => break,
                        }
                    }
                }
                Some(imsg) = self.internal_rx.recv() => {
                    self.handle_internal_msg(imsg);
                }
            }
        }

        if let Some(conn) = self.connecting.take() {
            conn.task.abort();
        }
        if let AppMode::Attached(ref state) = self.mode {
            let _ = self.client.detach_session(&state.session_id).await;
        }

        Ok(())
    }

    fn apply_block_stream_event(&mut self, block_id: &str, ev: TerminalStreamEvent) {
        if let AppMode::Attached(ref mut state) = self.mode {
            if let Some(pane) = state.panes.get_mut(block_id) {
                pane.apply_stream_event(ev);
            }
        }
    }

    fn handle_internal_msg(&mut self, msg: InternalMsg) {
        match msg {
            InternalMsg::SessionViewLoaded { host_label, view } => {
                if host_label == self.server.host_label {
                    self.session_previews.insert(view.session_id.clone(), view);
                }
            }
            InternalMsg::AttachedViewUpdated(view) => {
                if let AppMode::Attached(ref mut state) = self.mode {
                    if state.session_id == view.session_id && view.revision >= state.view.revision {
                        state.view = view;
                        self.sync_attached_panes();
                    }
                }
            }
            InternalMsg::AttachReady(view) => {
                self.enter_attached_view(view);
            }
            InternalMsg::StatusError(err) => {
                self.status_msg = Some((err, true));
            }
            InternalMsg::StatusInfo(info) => {
                self.status_msg = Some((info, false));
            }
            InternalMsg::HostsReloaded(new_hosts) => {
                self.merge_loaded_hosts(new_hosts);
                if let ManagerModal::HostSwitcher { ref mut selected } = self.modal {
                    if *selected >= self.hosts.len() {
                        *selected = self.hosts.len().saturating_sub(1);
                    }
                }
            }
            InternalMsg::HostConnected {
                server,
                client,
                event_rx,
                sessions,
                adhoc_item,
            } => {
                self.connecting = None;
                self.mode = AppMode::SessionManager;
                self.modal = ManagerModal::None;
                let msg = format!(
                    "已连接 [{}] {} ({})",
                    server.transport_kind.badge(),
                    server.host_label,
                    server.endpoint_display
                );
                if let Some(adhoc) = adhoc_item {
                    if !self
                        .hosts
                        .iter()
                        .any(|h| h.label.eq_ignore_ascii_case(&adhoc.label))
                    {
                        self.hosts.push(adhoc);
                    }
                }
                self.server = server;
                self.client = client;
                self.event_rx = event_rx;
                self.sessions = sessions;
                self.selected_idx = 0;
                self.session_previews.clear();
                self.status_msg = Some((msg, false));
                self.request_selected_preview();
            }
            InternalMsg::HostConnectFailed(err) => {
                self.connecting = None;
                self.status_msg = Some((err, true));
            }
        }
    }

    async fn handle_rex_event(&mut self, ev: RexEvent) {
        match ev {
            RexEvent::SessionCreated(summary) => {
                if !self
                    .sessions
                    .iter()
                    .any(|s| s.session_id == summary.session_id)
                {
                    self.sessions.push(summary);
                }
                self.request_selected_preview();
            }
            RexEvent::SessionDestroyed { session_id } => {
                self.sessions.retain(|s| s.session_id != session_id);
                self.session_previews.remove(&session_id);
                self.clamp_selection();

                let detached_from_destroyed = matches!(
                    &self.mode,
                    AppMode::Attached(st) if st.session_id == session_id
                );
                if detached_from_destroyed {
                    self.mode = AppMode::SessionManager;
                    self.status_msg = Some(("当前连接的会话已结束，已返回会话列表".into(), false));
                    self.request_selected_preview();
                } else if matches!(self.mode, AppMode::SessionManager) {
                    self.request_selected_preview();
                }
            }
            RexEvent::SessionsChanged => {
                let client = self.client.clone();
                if let Ok(list) = client.list_sessions().await {
                    self.sessions = list;
                    self.clamp_selection();
                }
            }
            RexEvent::HostsChanged => {
                self.reload_hosts_bg();
            }
            RexEvent::SessionLabelChanged { session_id, label } => {
                for s in &mut self.sessions {
                    if s.session_id == session_id {
                        s.label = label.clone();
                    }
                }
                if let Some(prev) = self.session_previews.get_mut(&session_id) {
                    prev.label = label.clone();
                }
                if let AppMode::Attached(ref mut st) = self.mode {
                    if st.session_id == session_id {
                        st.view.label = label;
                    }
                }
            }
            RexEvent::SessionViewChanged {
                session_id,
                revision,
            } => match &self.mode {
                AppMode::Attached(st) if st.session_id == session_id => {
                    if revision > st.view.revision {
                        let client = self.client.clone();
                        let tx = self.internal_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(v) = client.session_view(&session_id).await {
                                let _ = tx.send(InternalMsg::AttachedViewUpdated(v));
                            }
                        });
                    }
                }
                AppMode::SessionManager => {
                    if self
                        .selected_session()
                        .map(|s| s.session_id == session_id)
                        .unwrap_or(false)
                    {
                        self.request_selected_preview();
                    }
                }
                _ => {}
            },
            RexEvent::BlockClosed {
                session_id,
                block_id,
            } => {
                if let AppMode::Attached(ref mut st) = self.mode {
                    if st.session_id == session_id {
                        st.panes.remove(&block_id);
                        st.owned_blocks.remove(&block_id);
                    }
                }
            }
            RexEvent::BlockProcessChanged {
                session_id,
                block_id,
                foreground_name,
            } => {
                if let AppMode::Attached(ref mut st) = self.mode {
                    if st.session_id == session_id {
                        if let Some(pane) = st.panes.get_mut(&block_id) {
                            pane.foreground_process = foreground_name;
                        }
                    }
                }
            }
            RexEvent::BlockTitleChanged {
                session_id,
                block_id,
                title,
            } => {
                if let AppMode::Attached(ref mut st) = self.mode {
                    if st.session_id == session_id {
                        if let Some(pane) = st.panes.get_mut(&block_id) {
                            pane.title = if title.is_empty() { None } else { Some(title) };
                        }
                    }
                }
            }
            RexEvent::Disconnected(reason) => {
                self.mode = AppMode::SessionManager;
                self.status_msg = Some((
                    format!(
                        "与主机 {} ({}) 断开连接: {}（按 s 可重新选择或重连主机）",
                        self.server.host_label, self.server.endpoint_display, reason
                    ),
                    true,
                ));
            }
        }
    }

    pub async fn attach_to_session(&mut self, session_id: &str) -> Result<()> {
        let view = self.client.attach_session(session_id).await?;
        self.enter_attached_view(view);
        Ok(())
    }

    fn enter_attached_view(&mut self, view: SessionView) {
        let sid = view.session_id.clone();
        self.session_previews.insert(sid.clone(), view.clone());
        self.mode = AppMode::Attached(Box::new(AttachedState {
            session_id: sid,
            view,
            panes: HashMap::new(),
            owned_blocks: HashSet::new(),
            prefix_active: false,
            scroll_mode: false,
            show_help: false,
            tab_hitboxes: Vec::new(),
        }));
        self.modal = ManagerModal::None;
        self.status_msg = None;
        self.sync_attached_panes();
    }

    pub fn attached_pane_area(&self) -> Rect {
        if self.term_size.height <= 1 {
            return Rect::new(0, 0, self.term_size.width, 0);
        }
        Rect::new(0, 1, self.term_size.width, self.term_size.height - 1)
    }

    pub fn compute_active_placements(&self) -> Vec<PanePlacement> {
        let AppMode::Attached(ref state) = self.mode else {
            return Vec::new();
        };
        let Some(win) = state.view.active_window() else {
            return Vec::new();
        };
        compute_window_panes(win, self.attached_pane_area())
    }

    pub fn sync_attached_panes(&mut self) {
        let pane_area = self.attached_pane_area();
        let AppMode::Attached(ref mut state) = self.mode else {
            return;
        };
        let Some(win) = state.view.active_window().cloned() else {
            return;
        };

        let placements = compute_window_panes(&win, pane_area);

        for p in placements {
            if p.summary.creator_name != "com.superlogical.terminal" {
                continue;
            }
            let cols = p.inner_rect.width.max(2);
            let rows = p.inner_rect.height.max(1);
            let needs_own = state.owned_blocks.insert(p.block_id.clone());

            if !state.panes.contains_key(&p.block_id) {
                let mut pane = TerminalPane::new(cols, rows);
                let client = self.client.clone();
                let sid = state.session_id.clone();
                let bid = p.block_id.clone();
                let stream_tx = self.stream_tx.clone();

                let join = tokio::spawn(async move {
                    if needs_own {
                        let _ = client.own_blocks(&sid, std::slice::from_ref(&bid)).await;
                    }
                    let _ = client.terminal_resize(&sid, &bid, cols, rows).await;
                    let _ = run_terminal_stream(client, sid, bid, stream_tx).await;
                });
                pane.task_handle = Some(join);
                state.panes.insert(p.block_id.clone(), pane);
            } else if let Some(pane) = state.panes.get_mut(&p.block_id) {
                pane.resize_local(cols, rows);
                if pane.requested_size != (cols, rows) || needs_own {
                    pane.requested_size = (cols, rows);
                    let client = self.client.clone();
                    let sid = state.session_id.clone();
                    let bid = p.block_id.clone();
                    tokio::spawn(async move {
                        if needs_own {
                            let _ = client.own_blocks(&sid, std::slice::from_ref(&bid)).await;
                        }
                        let _ = client.terminal_resize(&sid, &bid, cols, rows).await;
                    });
                }
            }
        }
    }

    async fn handle_crossterm_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Release {
                    return;
                }
                match self.mode {
                    AppMode::SessionManager => self.handle_manager_key(key).await,
                    AppMode::Attached(_) => self.handle_attached_key(key).await,
                }
            }
            Event::Paste(text) => {
                if let AppMode::Attached(ref mut state) = self.mode {
                    state.prefix_active = false;
                    if state.scroll_mode {
                        state.scroll_mode = false;
                    }
                    if let Some(win) = state.view.active_window() {
                        if let Some(ref bid) = win.focused_block_id {
                            let bracketed = state
                                .panes
                                .get(bid)
                                .map(|p| p.bracketed_paste())
                                .unwrap_or(false);
                            if let Some(pane) = state.panes.get_mut(bid) {
                                pane.reset_scrollback();
                            }
                            let encoded = encode_paste_to_vt(&text, bracketed);
                            let _ = self.write_tx.send((
                                self.client.clone(),
                                state.session_id.clone(),
                                bid.clone(),
                                encoded,
                            ));
                        }
                    }
                } else {
                    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                    match &mut self.modal {
                        ManagerModal::NewSession { input, .. }
                        | ManagerModal::RenameSession { input, .. } => {
                            input.push_str(&clean);
                        }
                        ManagerModal::QuickConnect { endpoint } => {
                            endpoint.push_str(&clean);
                        }
                        ManagerModal::AddHost {
                            label,
                            endpoint,
                            focus_field,
                            ..
                        } => {
                            if *focus_field == 0 {
                                label.push_str(&clean);
                            } else {
                                endpoint.push_str(&clean);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Mouse(mev) => {
                self.handle_mouse_event(mev).await;
            }
            Event::Resize(cols, rows) => {
                self.term_size = Rect::new(0, 0, cols, rows);
                self.sync_attached_panes();
            }
            _ => {}
        }
    }

    async fn handle_mouse_event(&mut self, mev: MouseEvent) {
        match mev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let AppMode::Attached(ref state) = self.mode {
                    if state.show_help {
                        if let AppMode::Attached(ref mut st) = self.mode {
                            st.show_help = false;
                        }
                        return;
                    }
                    if mev.row == 0 {
                        for (x0, x1, wid) in &state.tab_hitboxes {
                            if mev.column >= *x0 && mev.column < *x1 {
                                let client = self.client.clone();
                                let sid = state.session_id.clone();
                                let wid = wid.clone();
                                tokio::spawn(async move {
                                    let _ = client.focus_window(&sid, &wid).await;
                                });
                                return;
                            }
                        }
                    } else {
                        let placements = self.compute_active_placements();
                        for p in placements.iter().rev() {
                            let r = p.outer_rect;
                            if mev.column >= r.x
                                && mev.column < r.x + r.width
                                && mev.row >= r.y
                                && mev.row < r.y + r.height
                            {
                                if !p.is_focused {
                                    if let AppMode::Attached(ref st) = self.mode {
                                        let client = self.client.clone();
                                        let sid = st.session_id.clone();
                                        let bid = p.block_id.clone();
                                        tokio::spawn(async move {
                                            let _ = client.focus_block(&sid, &bid).await;
                                        });
                                    }
                                }
                                break;
                            }
                        }
                    }
                } else if matches!(self.modal, ManagerModal::None) && self.connecting.is_none() {
                    // Click on Host Bar (row 3) switches host
                    if mev.row == 3 {
                        if let Some((_, _, idx)) = self
                            .host_hitboxes
                            .iter()
                            .find(|(x0, x1, _)| mev.column >= *x0 && mev.column < *x1)
                            .cloned()
                        {
                            if idx != self.active_host_index() {
                                if let Some(item) = self.hosts.get(idx).cloned() {
                                    self.start_connect_host_item(item);
                                }
                            }
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if let AppMode::Attached(ref mut state) = self.mode {
                    if state.scroll_mode || mev.modifiers.contains(KeyModifiers::SHIFT) {
                        if let Some(bid) = state
                            .view
                            .active_window()
                            .and_then(|w| w.focused_block_id.as_ref())
                        {
                            if let Some(pane) = state.panes.get_mut(bid) {
                                pane.scroll_up(3);
                                state.scroll_mode = pane.parser.screen().scrollback() > 0;
                            }
                        }
                    }
                } else if matches!(self.modal, ManagerModal::None) && self.selected_idx > 0 {
                    self.selected_idx -= 1;
                    self.request_selected_preview();
                }
            }
            MouseEventKind::ScrollDown => {
                if let AppMode::Attached(ref mut state) = self.mode {
                    if state.scroll_mode || mev.modifiers.contains(KeyModifiers::SHIFT) {
                        if let Some(bid) = state
                            .view
                            .active_window()
                            .and_then(|w| w.focused_block_id.as_ref())
                        {
                            if let Some(pane) = state.panes.get_mut(bid) {
                                pane.scroll_down(3);
                                state.scroll_mode = pane.parser.screen().scrollback() > 0;
                            }
                        }
                    }
                } else if matches!(self.modal, ManagerModal::None)
                    && self.selected_idx + 1 < self.sessions.len()
                {
                    self.selected_idx += 1;
                    self.request_selected_preview();
                }
            }
            _ => {}
        }
    }

    async fn handle_manager_key(&mut self, key: KeyEvent) {
        // Allow canceling an in-flight async host connection with Esc or Ctrl+C
        if self.connecting.is_some() {
            if key.code == KeyCode::Esc
                || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
            {
                if let Some(conn) = self.connecting.take() {
                    conn.task.abort();
                }
                self.status_msg = Some(("已取消主机连接".into(), false));
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if !matches!(self.modal, ManagerModal::None) {
                self.modal = ManagerModal::None;
            } else {
                self.should_quit = true;
            }
            return;
        }

        match self.modal.clone() {
            ManagerModal::Help => {
                self.modal = ManagerModal::None;
                return;
            }
            ManagerModal::HostSwitcher { mut selected } => {
                let count = self.hosts.len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.modal = ManagerModal::None;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if count > 0 {
                            selected = (selected + 1) % count;
                            self.modal = ManagerModal::HostSwitcher { selected };
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if count > 0 {
                            selected = if selected == 0 { count - 1 } else { selected - 1 };
                            self.modal = ManagerModal::HostSwitcher { selected };
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(item) = self.hosts.get(selected).cloned() {
                            self.modal = ManagerModal::None;
                            self.start_connect_host_item(item);
                        }
                    }
                    KeyCode::Char('a') | KeyCode::Char('n') => {
                        self.modal = ManagerModal::AddHost {
                            label: String::new(),
                            endpoint: String::new(),
                            focus_field: 0,
                            connect_now: true,
                        };
                    }
                    KeyCode::Char('c') => {
                        self.modal = ManagerModal::QuickConnect {
                            endpoint: String::new(),
                        };
                    }
                    KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => {
                        if let Some(item) = self.hosts.get(selected).cloned() {
                            if item.is_local {
                                self.status_msg = Some(("本机 (local) 不可删除".into(), true));
                            } else {
                                self.modal = ManagerModal::ConfirmDeleteHost {
                                    label: item.label,
                                    is_adhoc: item.is_adhoc,
                                };
                            }
                        }
                    }
                    KeyCode::Char('R') => {
                        self.reload_hosts_bg();
                    }
                    _ => {}
                }
                return;
            }
            ManagerModal::AddHost {
                mut label,
                mut endpoint,
                mut focus_field,
                mut connect_now,
            } => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
                    connect_now = !connect_now;
                    self.modal = ManagerModal::AddHost {
                        label,
                        endpoint,
                        focus_field,
                        connect_now,
                    };
                    return;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
                    if focus_field == 0 {
                        label.clear();
                    } else {
                        endpoint.clear();
                    }
                    self.modal = ManagerModal::AddHost {
                        label,
                        endpoint,
                        focus_field,
                        connect_now,
                    };
                    return;
                }
                match key.code {
                    KeyCode::Esc => {
                        self.modal = ManagerModal::HostSwitcher {
                            selected: self.active_host_index(),
                        };
                    }
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                        focus_field = if focus_field == 0 { 1 } else { 0 };
                        self.modal = ManagerModal::AddHost {
                            label,
                            endpoint,
                            focus_field,
                            connect_now,
                        };
                    }
                    KeyCode::Backspace => {
                        if focus_field == 0 {
                            label.pop();
                        } else {
                            endpoint.pop();
                        }
                        self.modal = ManagerModal::AddHost {
                            label,
                            endpoint,
                            focus_field,
                            connect_now,
                        };
                    }
                    KeyCode::Enter => {
                        if focus_field == 0 && endpoint.trim().is_empty() {
                            focus_field = 1;
                            self.modal = ManagerModal::AddHost {
                                label,
                                endpoint,
                                focus_field,
                                connect_now,
                            };
                            return;
                        }
                        let eps: Vec<String> = endpoint
                            .split(|c: char| c.is_whitespace() || c == ',')
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                            .collect();
                        let lbl = label.trim().to_string();
                        match add_rex_host(&lbl, &eps).await {
                            Ok(()) => {
                                let new_item = HostItem {
                                    label: lbl.clone(),
                                    endpoints: eps,
                                    is_local: false,
                                    is_adhoc: false,
                                };
                                self.merge_loaded_hosts(load_host_items().await);
                                if !self
                                    .hosts
                                    .iter()
                                    .any(|h| h.label.eq_ignore_ascii_case(&lbl))
                                {
                                    self.hosts.push(new_item.clone());
                                }
                                if connect_now {
                                    self.modal = ManagerModal::None;
                                    self.start_connect_host_item(new_item);
                                } else {
                                    let idx = self
                                        .hosts
                                        .iter()
                                        .position(|h| h.label.eq_ignore_ascii_case(&lbl))
                                        .unwrap_or(0);
                                    self.modal = ManagerModal::HostSwitcher { selected: idx };
                                    self.status_msg =
                                        Some((format!("已保存主机 \"{}\"", lbl), false));
                                }
                            }
                            Err(e) => {
                                self.status_msg = Some((format!("添加主机失败: {}", e), true));
                            }
                        }
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        if focus_field == 0 {
                            label.push(c);
                        } else {
                            endpoint.push(c);
                        }
                        self.modal = ManagerModal::AddHost {
                            label,
                            endpoint,
                            focus_field,
                            connect_now,
                        };
                    }
                    _ => {}
                }
                return;
            }
            ManagerModal::QuickConnect { mut endpoint } => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
                    endpoint.clear();
                    self.modal = ManagerModal::QuickConnect { endpoint };
                    return;
                }
                match key.code {
                    KeyCode::Esc => {
                        self.modal = ManagerModal::None;
                    }
                    KeyCode::Backspace => {
                        endpoint.pop();
                        self.modal = ManagerModal::QuickConnect { endpoint };
                    }
                    KeyCode::Enter => {
                        if !endpoint.trim().is_empty() {
                            self.modal = ManagerModal::None;
                            self.start_connect_adhoc(endpoint);
                        }
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        endpoint.push(c);
                        self.modal = ManagerModal::QuickConnect { endpoint };
                    }
                    _ => {}
                }
                return;
            }
            ManagerModal::ConfirmDeleteHost { label, is_adhoc } => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        if is_adhoc {
                            self.hosts
                                .retain(|h| !h.label.eq_ignore_ascii_case(&label));
                            self.modal = ManagerModal::HostSwitcher { selected: 0 };
                            self.status_msg =
                                Some((format!("已移除临时主机 \"{}\"", label), false));
                        } else {
                            match remove_rex_host(&label).await {
                                Ok(()) => {
                                    self.merge_loaded_hosts(load_host_items().await);
                                    self.modal = ManagerModal::HostSwitcher { selected: 0 };
                                    self.status_msg =
                                        Some((format!("已移除主机 \"{}\"", label), false));
                                }
                                Err(e) => {
                                    self.modal = ManagerModal::HostSwitcher { selected: 0 };
                                    self.status_msg =
                                        Some((format!("移除主机失败: {}", e), true));
                                }
                            }
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.modal = ManagerModal::HostSwitcher {
                            selected: self.active_host_index(),
                        };
                    }
                    _ => {}
                }
                return;
            }
            ManagerModal::NewSession {
                mut input,
                attach_immediately,
            } => {
                match key.code {
                    KeyCode::Esc => self.modal = ManagerModal::None,
                    KeyCode::Enter => {
                        self.modal = ManagerModal::None;
                        let client = self.client.clone();
                        let lbl = if input.trim().is_empty() {
                            None
                        } else {
                            Some(input.trim().to_string())
                        };
                        match client.create_session(lbl.as_deref()).await {
                            Ok(created) => {
                                if let Ok(list) = client.list_sessions().await {
                                    self.sessions = list;
                                    if let Some(idx) = self
                                        .sessions
                                        .iter()
                                        .position(|s| s.session_id == created.session_id)
                                    {
                                        self.selected_idx = idx;
                                    }
                                }
                                if attach_immediately {
                                    if let Err(e) =
                                        self.attach_to_session(&created.session_id).await
                                    {
                                        self.status_msg =
                                            Some((format!("连接新建会话失败: {}", e), true));
                                    }
                                } else {
                                    self.status_msg = Some(("已创建新会话".into(), false));
                                    self.request_selected_preview();
                                }
                            }
                            Err(e) => {
                                self.status_msg = Some((format!("创建会话失败: {}", e), true));
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        self.modal = ManagerModal::NewSession {
                            input,
                            attach_immediately,
                        };
                    }
                    KeyCode::Tab => {
                        self.modal = ManagerModal::NewSession {
                            input,
                            attach_immediately: !attach_immediately,
                        };
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        input.push(c);
                        self.modal = ManagerModal::NewSession {
                            input,
                            attach_immediately,
                        };
                    }
                    _ => {}
                }
                return;
            }
            ManagerModal::RenameSession {
                session_id,
                mut input,
            } => {
                match key.code {
                    KeyCode::Esc => self.modal = ManagerModal::None,
                    KeyCode::Enter => {
                        self.modal = ManagerModal::None;
                        match self.client.rename_session(&session_id, input.trim()).await {
                            Ok(()) => {
                                if let Ok(list) = self.client.list_sessions().await {
                                    self.sessions = list;
                                    self.clamp_selection();
                                }
                                self.request_selected_preview();
                                self.status_msg = Some(("会话已重命名".into(), false));
                            }
                            Err(e) => {
                                self.status_msg = Some((format!("重命名失败: {}", e), true));
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        self.modal = ManagerModal::RenameSession { session_id, input };
                    }
                    KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'u' => {
                        input.clear();
                        self.modal = ManagerModal::RenameSession { session_id, input };
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        input.push(c);
                        self.modal = ManagerModal::RenameSession { session_id, input };
                    }
                    _ => {}
                }
                return;
            }
            ManagerModal::ConfirmDelete { session_id, label } => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        self.modal = ManagerModal::None;
                        match self.client.destroy_session(&session_id).await {
                            Ok(()) => {
                                self.sessions.retain(|s| s.session_id != session_id);
                                self.session_previews.remove(&session_id);
                                self.clamp_selection();
                                self.status_msg =
                                    Some((format!("已关闭会话 \"{}\"", label), false));
                                self.request_selected_preview();
                            }
                            Err(e) => {
                                self.status_msg = Some((format!("关闭会话失败: {}", e), true));
                            }
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.modal = ManagerModal::None;
                    }
                    _ => {}
                }
                return;
            }
            ManagerModal::None => {}
        }

        // Normal Session Manager shortcuts
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.sessions.is_empty() {
                    self.selected_idx = (self.selected_idx + 1) % self.sessions.len();
                    self.request_selected_preview();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.sessions.is_empty() {
                    self.selected_idx = if self.selected_idx == 0 {
                        self.sessions.len() - 1
                    } else {
                        self.selected_idx - 1
                    };
                    self.request_selected_preview();
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                if !self.sessions.is_empty() {
                    self.selected_idx = 0;
                    self.request_selected_preview();
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.sessions.is_empty() {
                    self.selected_idx = self.sessions.len() - 1;
                    self.request_selected_preview();
                }
            }
            KeyCode::Enter => {
                if let Some(sess) = self.selected_session().cloned() {
                    let tx = self.internal_tx.clone();
                    let client = self.client.clone();
                    let sid = sess.session_id;
                    tokio::spawn(async move {
                        match client.attach_session(&sid).await {
                            Ok(view) => {
                                let _ = tx.send(InternalMsg::AttachReady(view));
                            }
                            Err(e) => {
                                let _ = tx.send(InternalMsg::StatusError(format!(
                                    "连接会话失败: {}",
                                    e
                                )));
                            }
                        }
                    });
                } else {
                    self.modal = ManagerModal::NewSession {
                        input: String::new(),
                        attach_immediately: true,
                    };
                }
            }
            KeyCode::Char('n') => {
                self.modal = ManagerModal::NewSession {
                    input: String::new(),
                    attach_immediately: false,
                };
            }
            KeyCode::Char('N') | KeyCode::Char('a') => {
                self.modal = ManagerModal::NewSession {
                    input: String::new(),
                    attach_immediately: true,
                };
            }
            KeyCode::Char('r') => {
                if let Some(sess) = self.selected_session().cloned() {
                    self.modal = ManagerModal::RenameSession {
                        session_id: sess.session_id,
                        input: sess.label,
                    };
                }
            }
            KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(sess) = self.selected_session().cloned() {
                    let display = if sess.label.is_empty() {
                        sess.session_id.clone()
                    } else {
                        sess.label
                    };
                    self.modal = ManagerModal::ConfirmDelete {
                        session_id: sess.session_id,
                        label: display,
                    };
                }
            }
            // Multi-host management & quick switching
            KeyCode::Char('s') | KeyCode::Char('H') => {
                let sel = self.active_host_index();
                self.reload_hosts_bg();
                self.modal = ManagerModal::HostSwitcher { selected: sel };
            }
            KeyCode::Char('c') => {
                self.modal = ManagerModal::QuickConnect {
                    endpoint: String::new(),
                };
            }
            KeyCode::Char(']') => {
                if self.hosts.len() > 1 {
                    let next = (self.active_host_index() + 1) % self.hosts.len();
                    if let Some(item) = self.hosts.get(next).cloned() {
                        self.start_connect_host_item(item);
                    }
                }
            }
            KeyCode::Char('[') => {
                if self.hosts.len() > 1 {
                    let cur = self.active_host_index();
                    let prev = if cur == 0 {
                        self.hosts.len() - 1
                    } else {
                        cur - 1
                    };
                    if let Some(item) = self.hosts.get(prev).cloned() {
                        self.start_connect_host_item(item);
                    }
                }
            }
            KeyCode::Char('R') => {
                self.reload_hosts_bg();
                match self.client.list_sessions().await {
                    Ok(list) => {
                        self.sessions = list;
                        self.clamp_selection();
                        self.request_selected_preview();
                        let _ = self.internal_tx.send(InternalMsg::StatusInfo(
                            "已刷新会话与主机列表".into(),
                        ));
                    }
                    Err(e) => {
                        self.status_msg = Some((format!("刷新失败: {}", e), true));
                    }
                }
            }
            KeyCode::Char('?') => {
                self.modal = ManagerModal::Help;
            }
            _ => {}
        }
    }

    async fn handle_attached_key(&mut self, key: KeyEvent) {
        let page = self.attached_pane_area().height.saturating_sub(4).max(5) as usize;
        let AppMode::Attached(ref mut state) = self.mode else {
            return;
        };

        if state.show_help {
            state.show_help = false;
            return;
        }

        if state.scroll_mode && !state.prefix_active {
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('b') {
                state.prefix_active = true;
                return;
            }
            let focused_bid = state
                .view
                .active_window()
                .and_then(|w| w.focused_block_id.clone());
            if let Some(bid) = focused_bid {
                if let Some(pane) = state.panes.get_mut(&bid) {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => pane.scroll_up(1),
                        KeyCode::Down | KeyCode::Char('j') => pane.scroll_down(1),
                        KeyCode::PageUp | KeyCode::Char('b') => pane.scroll_up(page),
                        KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char(' ') => {
                            pane.scroll_down(page);
                        }
                        KeyCode::Char('u') => pane.scroll_up(page / 2),
                        KeyCode::Char('d') => pane.scroll_down(page / 2),
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                            pane.reset_scrollback();
                            state.scroll_mode = false;
                        }
                        _ => {}
                    }
                    if pane.parser.screen().scrollback() == 0
                        && matches!(
                            key.code,
                            KeyCode::Down
                                | KeyCode::PageDown
                                | KeyCode::Char('j')
                                | KeyCode::Char('f')
                                | KeyCode::Char('d')
                        )
                    {
                        state.scroll_mode = false;
                    }
                } else {
                    state.scroll_mode = false;
                }
            } else {
                state.scroll_mode = false;
            }
            return;
        }

        if !state.prefix_active
            && key.modifiers == KeyModifiers::CONTROL
            && (key.code == KeyCode::Char('b') || key.code == KeyCode::Char('B'))
        {
            state.prefix_active = true;
            return;
        }

        if state.prefix_active {
            state.prefix_active = false;
            let sid = state.session_id.clone();
            let client = self.client.clone();

            if key.modifiers.contains(KeyModifiers::CONTROL)
                && (key.code == KeyCode::Char('b') || key.code == KeyCode::Char('B'))
            {
                self.write_to_focused_block("\x02");
                return;
            }

            match key.code {
                KeyCode::Char('d') => {
                    let label = if state.view.label.is_empty() {
                        sid.clone()
                    } else {
                        state.view.label.clone()
                    };
                    self.mode = AppMode::SessionManager;
                    let _ = client.detach_session(&sid).await;
                    if let Ok(list) = client.list_sessions().await {
                        self.sessions = list;
                        if let Some(idx) = self.sessions.iter().position(|s| s.session_id == sid) {
                            self.selected_idx = idx;
                        }
                        self.clamp_selection();
                    }
                    self.status_msg =
                        Some((format!("已从会话 \"{}\" 分离 (Detached)", label), false));
                    self.request_selected_preview();
                }
                KeyCode::Char('n') | KeyCode::Tab => {
                    tokio::spawn(async move {
                        let _ = client.focus_next_window(&sid).await;
                    });
                }
                KeyCode::Char('p') | KeyCode::BackTab => {
                    tokio::spawn(async move {
                        let _ = client.focus_previous_window(&sid).await;
                    });
                }
                KeyCode::Char(d @ '1'..='9') => {
                    let idx = (d as usize) - ('1' as usize);
                    if let Some(win) = state.view.windows.get(idx) {
                        let wid = win.window_id.clone();
                        tokio::spawn(async move {
                            let _ = client.focus_window(&sid, &wid).await;
                        });
                    }
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    tokio::spawn(async move {
                        let _ = client.focus_direction(&sid, "left").await;
                    });
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    tokio::spawn(async move {
                        let _ = client.focus_direction(&sid, "right").await;
                    });
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    tokio::spawn(async move {
                        let _ = client.focus_direction(&sid, "up").await;
                    });
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    tokio::spawn(async move {
                        let _ = client.focus_direction(&sid, "down").await;
                    });
                }
                KeyCode::Char('o') => {
                    tokio::spawn(async move {
                        let _ = client.focus_next_pane(&sid).await;
                    });
                }
                KeyCode::Char(';') => {
                    tokio::spawn(async move {
                        let _ = client.focus_previous_pane(&sid).await;
                    });
                }
                KeyCode::Char('z') => {
                    if let Some(win) = state.view.active_window() {
                        if let Some(ref bid) = win.focused_block_id {
                            let is_zoomed = win.zoomed_block_id.as_deref() == Some(bid.as_str());
                            let bid = bid.clone();
                            tokio::spawn(async move {
                                let _ = client.toggle_zoom_block(&sid, &bid, is_zoomed).await;
                            });
                        }
                    }
                }
                KeyCode::Char('[') | KeyCode::PageUp => {
                    state.scroll_mode = true;
                    if matches!(key.code, KeyCode::PageUp) {
                        if let Some(bid) = state
                            .view
                            .active_window()
                            .and_then(|w| w.focused_block_id.clone())
                        {
                            if let Some(pane) = state.panes.get_mut(&bid) {
                                pane.scroll_up(10);
                            }
                        }
                    }
                }
                KeyCode::Char('?') => {
                    state.show_help = !state.show_help;
                }
                KeyCode::Esc => {}
                _ => {}
            }
            return;
        }

        let app_cursor = state
            .view
            .active_window()
            .and_then(|w| w.focused_block_id.as_ref())
            .and_then(|bid| state.panes.get(bid))
            .map(|p| p.application_cursor())
            .unwrap_or(false);

        if let Some(vt_seq) = encode_key_to_vt(key, app_cursor) {
            self.write_to_focused_block(&vt_seq);
        }
    }

    fn write_to_focused_block(&mut self, data: &str) {
        let AppMode::Attached(ref mut state) = self.mode else {
            return;
        };
        let Some(win) = state.view.active_window() else {
            return;
        };
        let Some(ref bid) = win.focused_block_id else {
            return;
        };
        if let Some(pane) = state.panes.get_mut(bid) {
            pane.reset_scrollback();
        }
        let _ = self.write_tx.send((
            self.client.clone(),
            state.session_id.clone(),
            bid.clone(),
            data.to_string(),
        ));
    }
}
