use crate::client::{spawn_terminal_stream, RexClient, TerminalStreamEvent};
use crate::discovery::ResolvedServer;
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
    Help,
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
    SessionViewLoaded(SessionView),
    AttachedViewUpdated(SessionView),
    StatusError(String),
    AttachReady(SessionView),
}

pub struct App {
    pub server: ResolvedServer,
    pub client: RexClient,
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
    write_tx: mpsc::UnboundedSender<(String, String, String)>,
}

impl App {
    pub async fn new(
        server: ResolvedServer,
        client: RexClient,
        event_rx: mpsc::UnboundedReceiver<RexEvent>,
        initial_session: Option<&str>,
    ) -> Result<Self> {
        let sessions = client.attach_all().await?;
        let (stream_tx, stream_rx) = mpsc::unbounded_channel();
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<(String, String, String)>();

        // Sequential + coalescing terminal input writer task:
        // Rex runs `com.superlogical.terminal.write` concurrently unless calls wait for the
        // previous response or coalesce bursts into a single call.
        let writer_client = client.clone();
        tokio::spawn(async move {
            let mut buffered_next: Option<(String, String, String)> = None;
            loop {
                let (sid, bid, mut batch) = match buffered_next.take() {
                    Some(item) => item,
                    None => match write_rx.recv().await {
                        Some(item) => item,
                        None => break,
                    },
                };
                // Coalesce any immediately queued keystrokes targeting the same block
                while let Ok((nsid, nbid, ndata)) = write_rx.try_recv() {
                    if nsid == sid && nbid == bid && batch.len() + ndata.len() < 65536 {
                        batch.push_str(&ndata);
                    } else {
                        buffered_next = Some((nsid, nbid, ndata));
                        break;
                    }
                }
                let _ = writer_client.terminal_write(&sid, &bid, &batch).await;
            }
        });

        let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));

        let mut app = Self {
            server,
            client,
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
                app.status_msg = Some((format!("未找到会话 \"{}\"，已打开会话管理列表", target), true));
                app.request_selected_preview();
            }
        } else {
            app.request_selected_preview();
        }

        Ok(app)
    }

    fn find_session_by_query(&self, query: &str) -> Option<SessionSummary> {
        self.sessions
            .iter()
            .find(|s| s.session_id == query || s.label == query)
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|s| s.session_id.ends_with(query))
            })
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
        let client = self.client.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            // Attach briefly or read session.view (connection called attach_all, so session.view works!)
            if let Ok(view) = client.session_view(&sid).await {
                let _ = tx.send(InternalMsg::SessionViewLoaded(view));
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
                    // Drain any immediately queued events before redrawing
                    while let Ok(more) = self.event_rx.try_recv() {
                        self.handle_rex_event(more).await;
                    }
                }
                Some((block_id, stream_ev)) = self.stream_rx.recv() => {
                    self.apply_block_stream_event(&block_id, stream_ev);
                    // Batch-drain pending terminal output chunks for smooth high-throughput rendering
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

        // Cleanly detach if exiting while attached
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
            InternalMsg::SessionViewLoaded(view) => {
                self.session_previews.insert(view.session_id.clone(), view);
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
        }
    }

    async fn handle_rex_event(&mut self, ev: RexEvent) {
        match ev {
            RexEvent::SessionCreated(summary) => {
                if !self.sessions.iter().any(|s| s.session_id == summary.session_id) {
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
            RexEvent::SessionViewChanged { session_id, revision } => {
                match &self.mode {
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
                }
            }
            RexEvent::BlockClosed { session_id, block_id } => {
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
                self.status_msg = Some((format!("与本地 Rex Server 断开连接: {}", reason), true));
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

        // Ensure ownership, PTY dimensions, and live data stream match each visible terminal pane
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
                    if let Ok(inner_join) =
                        spawn_terminal_stream(client, sid, bid, stream_tx).await
                    {
                        let _ = inner_join.await;
                    }
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
                // Ignore key release events on terminals supporting kitty keyboard enhancements
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
                                state.session_id.clone(),
                                bid.clone(),
                                encoded,
                            ));
                        }
                    }
                } else {
                    match &mut self.modal {
                        ManagerModal::NewSession { input, .. }
                        | ManagerModal::RenameSession { input, .. } => {
                            let clean: String =
                                text.chars().filter(|c| !c.is_control()).collect();
                            input.push_str(&clean);
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
                    // Click on top tab bar switches window
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
                        // Click inside an unfocused pane focuses it
                        let placements = self.compute_active_placements();
                        // Iterate in reverse so floating layers on top win hit-testing
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
        // Global Ctrl+C in manager quits cleanly (or cancels modal)
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if !matches!(self.modal, ManagerModal::None) {
                self.modal = ManagerModal::None;
            } else {
                self.should_quit = true;
            }
            return;
        }

        // Modal input handling first
        match self.modal.clone() {
            ManagerModal::Help => {
                self.modal = ManagerModal::None;
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
            KeyCode::Char('R') => {
                match self.client.list_sessions().await {
                    Ok(list) => {
                        self.sessions = list;
                        self.clamp_selection();
                        self.request_selected_preview();
                        self.status_msg = Some(("已刷新会话列表".into(), false));
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

        // 1. Dismiss help overlay on Esc / q / ? / Enter
        if state.show_help {
            state.show_help = false;
            return;
        }

        // 2. Scrollback browsing mode (`Ctrl-b [`)
        if state.scroll_mode && !state.prefix_active {
            // Allow `Ctrl-b` inside scroll mode too
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
                    if pane.parser.screen().scrollback() == 0 && matches!(key.code, KeyCode::Down | KeyCode::PageDown | KeyCode::Char('j') | KeyCode::Char('f') | KeyCode::Char('d')) {
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

        // 3. Prefix key (`Ctrl-b`) trigger
        if !state.prefix_active
            && key.modifiers == KeyModifiers::CONTROL
            && (key.code == KeyCode::Char('b') || key.code == KeyCode::Char('B'))
        {
            state.prefix_active = true;
            return;
        }

        // 4. Handle command following `Ctrl-b` prefix
        if state.prefix_active {
            state.prefix_active = false;
            let sid = state.session_id.clone();
            let client = self.client.clone();

            // `Ctrl-b Ctrl-b` -> send literal Ctrl-b (\x02) to focused terminal
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && (key.code == KeyCode::Char('b') || key.code == KeyCode::Char('B'))
            {
                self.write_to_focused_block("\x02");
                return;
            }

            match key.code {
                // Detach back to Session Manager
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
                    self.status_msg = Some((format!("已从会话 \"{}\" 分离 (Detached)", label), false));
                    self.request_selected_preview();
                }
                // Window navigation
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
                // Directional & cycle Pane focus navigation
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
                // Zoom toggle on focused pane
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
                // Enter scrollback mode
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
                // Toggle help overlay
                KeyCode::Char('?') => {
                    state.show_help = !state.show_help;
                }
                KeyCode::Esc => {}
                _ => {}
            }
            return;
        }

        // 5. Normal key -> encode VT sequence and forward to focused terminal block
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
            state.session_id.clone(),
            bid.clone(),
            data.to_string(),
        ));
    }
}
