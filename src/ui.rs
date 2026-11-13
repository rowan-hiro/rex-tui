use crate::app::{App, AppMode, AttachedState, ManagerModal};
use crate::discovery::TransportKind;
use crate::layout::compute_window_panes;
use crate::terminal_pane::render_vt100_screen;
use crate::types::{LayoutNode, SessionView};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

pub fn render(frame: &mut Frame, app: &mut App) {
    match app.mode {
        AppMode::SessionManager => render_session_manager(frame, app),
        AppMode::Attached(_) => render_attached_session(frame, app),
    }
}

fn transport_color(kind: &TransportKind) -> Color {
    match kind {
        TransportKind::Local | TransportKind::Unix | TransportKind::Pipe => Color::Cyan,
        TransportKind::Http => Color::LightGreen,
        TransportKind::Ssh => Color::LightMagenta,
    }
}

fn render_session_manager(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header bar
            Constraint::Length(1), // Host switcher bar
            Constraint::Min(5),    // Sessions + Inspector
            Constraint::Length(2), // Footer shortcuts
        ])
        .split(area);

    // 1. Header bar
    let pid_info = app
        .server
        .server_pid
        .map(|p| format!("pid {}", p))
        .unwrap_or_else(|| "在线".to_string());
    let plat_info = app
        .server
        .platform
        .as_deref()
        .map(|p| format!(" • {}", p))
        .unwrap_or_default();
    let ver_info = app
        .server
        .rex_version
        .as_deref()
        .map(|v| format!(" • rex {}", v))
        .unwrap_or_default();

    let badge_color = transport_color(&app.server.transport_kind);

    let header_line = Line::from(vec![
        Span::styled(
            " REX TUI ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!(" {} ", app.server.transport_kind.badge()),
            Style::default()
                .fg(Color::Black)
                .bg(badge_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            &app.server.host_label,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({})", app.server.endpoint_display),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            format!(
                "  •  {}{}{}  •  {} 个会话",
                pid_info,
                plat_info,
                ver_info,
                app.sessions.len()
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let header = Paragraph::new(header_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(header, chunks[0]);

    // 2. Host Switcher Bar (row 3)
    render_host_bar(frame, app, chunks[1]);

    // 3. Main body: Left session list + Right live session structure inspector
    let main_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[2]);

    render_session_list(frame, app, main_cols[0]);
    render_session_inspector(frame, app, main_cols[1]);

    // 4. Footer status + shortcuts
    let mut footer_lines = Vec::new();
    if let Some((ref msg, is_err)) = app.status_msg {
        let color = if is_err {
            Color::LightRed
        } else {
            Color::LightGreen
        };
        footer_lines.push(Line::from(vec![
            Span::styled(
                " 提示: ",
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(msg.as_str(), Style::default().fg(color)),
        ]));
    } else {
        footer_lines.push(Line::from(vec![
            key_badge("Enter"),
            Span::raw(" 连接  "),
            key_badge("n/a"),
            Span::raw(" 新建  "),
            key_badge("r"),
            Span::raw(" 重命名  "),
            key_badge("d"),
            Span::raw(" 关闭  "),
            key_badge("s"),
            Span::raw(" 主机管理  "),
            key_badge("c"),
            Span::raw(" 远程直连  "),
            key_badge("[ / ]"),
            Span::raw(" 切主机  "),
            key_badge("?"),
            Span::raw(" 帮助  "),
            key_badge("q"),
            Span::raw(" 退出"),
        ]));
    }
    if footer_lines.len() == 1 && app.status_msg.is_some() {
        footer_lines.push(Line::from(vec![
            key_badge("Enter"),
            Span::raw(" 连接  "),
            key_badge("n"),
            Span::raw(" 新建  "),
            key_badge("s"),
            Span::raw(" 主机管理  "),
            key_badge("c"),
            Span::raw(" 远程直连  "),
            key_badge("?"),
            Span::raw(" 帮助  "),
            key_badge("q"),
            Span::raw(" 退出"),
        ]));
    }
    frame.render_widget(Paragraph::new(footer_lines), chunks[3]);

    // 5. Modal overlays
    match &app.modal {
        ManagerModal::None => {}
        ManagerModal::HostSwitcher { selected } => {
            render_host_switcher_modal(frame, app, *selected, area);
        }
        ManagerModal::AddHost {
            label,
            endpoint,
            focus_field,
            connect_now,
        } => {
            render_add_host_modal(frame, label, endpoint, *focus_field, *connect_now, area);
        }
        ManagerModal::QuickConnect { endpoint } => {
            render_quick_connect_modal(frame, endpoint, area);
        }
        ManagerModal::ConfirmDeleteHost { label, .. } => {
            let popup = centered_rect(54, 7, area);
            frame.render_widget(Clear, popup);
            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw("  确定要从主机列表中移除 "),
                    Span::styled(
                        format!("\"{}\"", label),
                        Style::default()
                            .fg(Color::LightRed)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" 吗？"),
                ]),
                Line::from(Span::styled(
                    "  （仅移除本地主机配置，不影响远程运行中的会话）",
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  [y / Enter] 确认移除    [n / Esc] 取消",
                    Style::default().fg(Color::Yellow),
                )),
            ];
            let block = Block::default()
                .title(" 移除远程主机 ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightRed));
            frame.render_widget(Paragraph::new(lines).block(block), popup);
        }
        ManagerModal::NewSession {
            input,
            attach_immediately,
        } => {
            let popup = centered_rect(56, 9, area);
            frame.render_widget(Clear, popup);
            let mode_txt = if *attach_immediately {
                "[✓] 创建后立即进入会话 (Tab 切换)"
            } else {
                "[ ] 仅创建会话留在列表 (Tab 切换)"
            };
            let lines = vec![
                Line::from(Span::styled(
                    format!("在主机 [{}] 上新建会话（留空则自动命名）：", app.server.host_label),
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" > ", Style::default().fg(Color::Cyan)),
                    Span::styled(
                        input.as_str(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("█", Style::default().fg(Color::Cyan)),
                ]),
                Line::from(""),
                Line::from(Span::styled(mode_txt, Style::default().fg(Color::Yellow))),
                Line::from(Span::styled(
                    "Enter 确认  •  Esc 取消",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            let block = Block::default()
                .title(" 新建 Rex 会话 ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            frame.render_widget(Paragraph::new(lines).block(block), popup);
        }
        ManagerModal::RenameSession { input, .. } => {
            let popup = centered_rect(52, 8, area);
            frame.render_widget(Clear, popup);
            let lines = vec![
                Line::from(Span::styled(
                    "输入新的会话名称（Ctrl+U 清空）：",
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" > ", Style::default().fg(Color::Cyan)),
                    Span::styled(
                        input.as_str(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("█", Style::default().fg(Color::Cyan)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter 保存  •  Esc 取消",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            let block = Block::default()
                .title(" 重命名会话 ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            frame.render_widget(Paragraph::new(lines).block(block), popup);
        }
        ManagerModal::ConfirmDelete { label, .. } => {
            let popup = centered_rect(56, 7, area);
            frame.render_widget(Clear, popup);
            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw(format!("  确定要在 [{}] 上销毁会话 ", app.server.host_label)),
                    Span::styled(
                        format!("\"{}\"", label),
                        Style::default()
                            .fg(Color::LightRed)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" 吗？"),
                ]),
                Line::from(Span::styled(
                    "  会话内的所有窗口与终端进程都将被终止。",
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  [y / Enter] 确认销毁    [n / Esc] 取消",
                    Style::default().fg(Color::Yellow),
                )),
            ];
            let block = Block::default()
                .title(" 确认关闭会话 ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightRed));
            frame.render_widget(Paragraph::new(lines).block(block), popup);
        }
        ManagerModal::Help => {
            render_help_overlay(frame, area, false);
        }
    }

    // 6. Async Connecting overlay (drawn topmost when establishing HTTP/SSH connection)
    if let Some(ref conn) = app.connecting {
        let popup = centered_rect(64, 6, area);
        frame.render_widget(Clear, popup);
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  ⟳ 正在连接主机: ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    conn.description.as_str(),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  请稍候...  按 Esc 可随时取消",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let block = Block::default()
            .title(" 连接远程服务 ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        frame.render_widget(Paragraph::new(lines).block(block), popup);
    }
}

fn render_host_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    app.host_hitboxes.clear();
    let active_idx = app.active_host_index();

    let mut spans: Vec<Span> = Vec::new();
    let prefix = " 主机: ";
    let mut cur_x = prefix.width() as u16;
    spans.push(Span::styled(prefix, Style::default().fg(Color::DarkGray)));

    for (idx, h) in app.hosts.iter().enumerate() {
        let is_active = idx == active_idx;
        let proto = h.protocol_hint();
        let txt = if h.is_local {
            if is_active {
                " ● local ".to_string()
            } else {
                " ○ local ".to_string()
            }
        } else if is_active {
            format!(" ● {} ({}) ", h.label, proto)
        } else {
            format!(" ○ {} ({}) ", h.label, proto)
        };
        let w = txt.width() as u16;
        app.host_hitboxes
            .push((cur_x, cur_x.saturating_add(w), idx));
        cur_x = cur_x.saturating_add(w);

        let style = if is_active {
            Style::default()
                .fg(Color::Black)
                .bg(transport_color(&app.server.transport_kind))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray).bg(Color::Rgb(30, 34, 42))
        };
        spans.push(Span::styled(txt, style));
        spans.push(Span::raw(" "));
        cur_x = cur_x.saturating_add(1);
    }

    spans.push(Span::styled(
        " (s: 管理/新增主机  c: 远程直连)",
        Style::default().fg(Color::DarkGray),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_host_switcher_modal(frame: &mut Frame, app: &App, selected: usize, area: Rect) {
    let popup = centered_rect(72, 16, area);
    frame.render_widget(Clear, popup);

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(popup);

    let active_idx = app.active_host_index();
    let items: Vec<ListItem> = app
        .hosts
        .iter()
        .enumerate()
        .map(|(idx, h)| {
            let is_sel = idx == selected;
            let is_conn = idx == active_idx;
            let cursor = if is_sel { "▸ " } else { "  " };
            let proto = h.protocol_hint().to_uppercase();
            let ep_txt = h.endpoints.join(", ");
            let active_tag = if is_conn { " [当前连接]" } else { "" };

            ListItem::new(Line::from(vec![
                Span::styled(
                    cursor,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<6} ", proto),
                    Style::default().fg(match proto.as_str() {
                        "SSH" => Color::LightMagenta,
                        "HTTP" => Color::LightGreen,
                        _ => Color::Cyan,
                    }),
                ),
                Span::styled(
                    format!("{:<14}", h.label),
                    Style::default().fg(Color::White).add_modifier(if is_sel {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::styled(ep_txt, Style::default().fg(Color::Gray)),
                Span::styled(active_tag, Style::default().fg(Color::Green)),
            ]))
        })
        .collect();

    let list_block = Block::default()
        .title(" 切换 / 管理远程主机 (兼容 rex hosts) ")
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::Cyan));
    let list = List::new(items).block(list_block).highlight_style(
        Style::default()
            .bg(Color::Rgb(28, 45, 65))
            .add_modifier(Modifier::BOLD),
    );
    let mut lstate = ListState::default();
    lstate.select(Some(selected));
    frame.render_stateful_widget(list, inner_chunks[0], &mut lstate);

    let help_block = Block::default()
        .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(Color::Cyan));
    let help_line = Line::from(vec![
        Span::raw(" "),
        key_badge("Enter"),
        Span::raw(" 连接  "),
        key_badge("a"),
        Span::raw(" 添加主机  "),
        key_badge("c"),
        Span::raw(" 临时直连  "),
        key_badge("d"),
        Span::raw(" 删除主机  "),
        key_badge("Esc"),
        Span::raw(" 关闭"),
    ]);
    frame.render_widget(Paragraph::new(help_line).block(help_block), inner_chunks[1]);
}

fn render_add_host_modal(
    frame: &mut Frame,
    label: &str,
    endpoint: &str,
    focus_field: u8,
    connect_now: bool,
    area: Rect,
) {
    let popup = centered_rect(68, 13, area);
    frame.render_widget(Clear, popup);

    let f0_color = if focus_field == 0 {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let f1_color = if focus_field == 1 {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let c0 = if focus_field == 0 { "█" } else { "" };
    let c1 = if focus_field == 1 { "█" } else { "" };
    let conn_txt = if connect_now {
        "[✓] 保存后立即连接到该主机 (Ctrl+T 切换)"
    } else {
        "[ ] 仅保存到主机列表 (Ctrl+T 切换)"
    };

    let lines = vec![
        Line::from(Span::styled(
            " 1. 主机别名 (如 devbox、homelab)：",
            Style::default().fg(f0_color),
        )),
        Line::from(vec![
            Span::styled("    > ", Style::default().fg(f0_color)),
            Span::styled(
                label,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(c0, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " 2. 连接地址 (支持 http://host:port 或 ssh://[user@]host[:port])：",
            Style::default().fg(f1_color),
        )),
        Line::from(vec![
            Span::styled("    > ", Style::default().fg(f1_color)),
            Span::styled(
                endpoint,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(c1, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(Span::styled(
            "    示例: ssh://ubuntu@192.168.1.20  或  http://100.64.0.8:7500",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" {}", conn_txt),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Tab/↑/↓ 切换输入框  •  Enter 保存  •  Esc 返回",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .title(" 添加远程 Rex 主机 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_quick_connect_modal(frame: &mut Frame, endpoint: &str, area: Rect) {
    let popup = centered_rect(66, 10, area);
    frame.render_widget(Clear, popup);

    let lines = vec![
        Line::from(Span::styled(
            "输入远程地址或主机名直接连接（支持 HTTP 与 SSH）：",
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(Color::Cyan)),
            Span::styled(
                endpoint,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " • HTTP 示例: http://192.168.1.10:7500",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            " • SSH  示例: ssh://user@192.168.1.10:22",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "Enter 立即连接  •  Ctrl+U 清空  •  Esc 取消",
            Style::default().fg(Color::Yellow),
        )),
    ];

    let block = Block::default()
        .title(" 快速直连远程机器 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_session_list(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(format!(
            " [{}] 会话列表 ({}) ",
            app.server.host_label,
            app.sessions.len()
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    if app.sessions.is_empty() {
        let empty_lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("主机 [{}] 当前暂无运行中的 Rex 会话", app.server.host_label),
                Style::default().fg(Color::Gray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("按 "),
                key_badge("Enter"),
                Span::raw(" 或 "),
                key_badge("n"),
                Span::raw(" 创建第一个会话，或按 "),
                key_badge("s"),
                Span::raw(" 切换主机"),
            ]),
        ];
        let p = Paragraph::new(empty_lines)
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let short_id = short_uuid(&s.session_id);
            let name = if s.label.is_empty() {
                "<未命名>"
            } else {
                s.label.as_str()
            };
            let counts = app
                .session_previews
                .get(&s.session_id)
                .map(|v| format!("{}W / {}P", v.windows.len(), v.total_blocks()))
                .unwrap_or_default();

            let is_sel = idx == app.selected_idx;
            let marker = if is_sel { "▸ " } else { "  " };
            let line = Line::from(vec![
                Span::styled(
                    marker,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<18}", name),
                    Style::default().fg(Color::White).add_modifier(if is_sel {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::raw(" "),
                Span::styled(format!("{:<9}", counts), Style::default().fg(Color::Yellow)),
                Span::styled(short_id, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::Rgb(28, 45, 65))
            .add_modifier(Modifier::BOLD),
    );
    let mut state = ListState::default();
    state.select(Some(app.selected_idx));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_session_inspector(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" 会话详情与窗口布局 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let Some(sess) = app.selected_session() else {
        frame.render_widget(
            Paragraph::new("\n  选择或创建一个会话以查看窗口与窗格布局")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };

    let Some(view) = app.session_previews.get(&sess.session_id) else {
        frame.render_widget(
            Paragraph::new(format!("\n  正在读取会话 {} 的布局信息...", sess.label))
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };

    let lines = build_inspector_lines(view);
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block);
    frame.render_widget(p, area);
}

fn build_inspector_lines(view: &SessionView) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" 名称: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            if view.label.is_empty() {
                "<未命名>".to_string()
            } else {
                view.label.clone()
            },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" ID:   ", Style::default().fg(Color::DarkGray)),
        Span::styled(view.session_id.clone(), Style::default().fg(Color::Gray)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" 统计: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{} 个窗口 (Windows)  •  {} 个窗格 (Blocks)  •  rev {}",
                view.windows.len(),
                view.total_blocks(),
                view.revision
            ),
            Style::default().fg(Color::White),
        ),
    ]));
    lines.push(Line::from(""));

    if view.windows.is_empty() {
        lines.push(Line::from(Span::styled(
            " (该会话当前没有打开的窗口)",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    for (i, win) in view.windows.iter().enumerate() {
        let is_active = view.active_window_id.as_deref() == Some(win.window_id.as_str());
        let wname = if win.label.is_empty() {
            format!("Window {}", i + 1)
        } else {
            win.label.clone()
        };
        let badge = if is_active { " [当前活动]" } else { "" };
        let zoom_badge = if win.zoomed_block_id.is_some() {
            " [已最大化]"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" ▣ #{}: {} ", i + 1, wname),
                Style::default()
                    .fg(if is_active { Color::Green } else { Color::White })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(badge.to_string(), Style::default().fg(Color::Green)),
            Span::styled(zoom_badge.to_string(), Style::default().fg(Color::Yellow)),
        ]));

        for layer in &win.layers {
            if let Some(ref root) = layer.layout {
                describe_layout_tree(
                    root,
                    &layer.blocks,
                    win.focused_block_id.as_deref(),
                    "    ",
                    &mut lines,
                );
            }
        }
        lines.push(Line::from(""));
    }

    lines
}

fn describe_layout_tree(
    node: &LayoutNode,
    blocks: &[crate::types::BlockSummary],
    focused_id: Option<&str>,
    indent: &str,
    out: &mut Vec<Line<'static>>,
) {
    if let Some(ref split) = node.split {
        let dir_txt = if split.direction == "vertical" {
            "上下分屏"
        } else {
            "左右分屏"
        };
        out.push(Line::from(Span::styled(
            format!(
                "{}├─ {} ({}% / {}%)",
                indent,
                dir_txt,
                (split.ratio * 100.0).round() as u32,
                ((1.0 - split.ratio) * 100.0).round() as u32
            ),
            Style::default().fg(Color::DarkGray),
        )));
        let next_indent = format!("{}│  ", indent);
        describe_layout_tree(&split.before, blocks, focused_id, &next_indent, out);
        describe_layout_tree(&split.after, blocks, focused_id, &next_indent, out);
    } else if let Some(ref bid) = node.block_id {
        let summary = blocks.iter().find(|b| &b.block_id == bid);
        let label = summary
            .and_then(|b| (!b.label.is_empty()).then_some(b.label.as_str()))
            .unwrap_or("terminal");
        let kind = summary
            .map(|b| {
                b.creator_name
                    .strip_prefix("com.superlogical.")
                    .unwrap_or(&b.creator_name)
            })
            .unwrap_or("block");
        let is_focus = focused_id == Some(bid.as_str());
        out.push(Line::from(vec![
            Span::styled(
                format!("{}└─ ", indent),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{} ", label),
                Style::default()
                    .fg(if is_focus { Color::Cyan } else { Color::Gray })
                    .add_modifier(if is_focus {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                format!("({}, {})", kind, short_uuid(bid)),
                Style::default().fg(Color::DarkGray),
            ),
            if is_focus {
                Span::styled(" ★焦点", Style::default().fg(Color::Cyan))
            } else {
                Span::raw("")
            },
        ]));
    }
}

fn render_attached_session(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    let top_area = Rect::new(0, 0, area.width, 1);
    let pane_area = app.attached_pane_area();
    let host_label = app.server.host_label.clone();
    let transport_kind = app.server.transport_kind.clone();

    let AppMode::Attached(ref mut state) = app.mode else {
        return;
    };

    render_attached_top_bar(frame, state, &host_label, &transport_kind, top_area);

    let Some(win) = state.view.active_window().cloned() else {
        let empty = Paragraph::new("\n  当前会话暂无活动窗口（可在其他客户端或通过 rex window new 创建）")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty, pane_area);
        if state.show_help {
            render_help_overlay(frame, area, true);
        }
        return;
    };

    let placements = compute_window_panes(&win, pane_area);
    let mut focused_cursor: Option<(u16, u16)> = None;

    for p in &placements {
        if p.is_floating {
            frame.render_widget(Clear, p.outer_rect);
        }

        let pane_state = state.panes.get(&p.block_id);
        let scrollback = pane_state
            .map(|tp| tp.parser.screen().scrollback())
            .unwrap_or(0);

        let border_color = if p.is_focused {
            if state.scroll_mode || scrollback > 0 {
                Color::Yellow
            } else if state.prefix_active {
                Color::LightMagenta
            } else {
                Color::Cyan
            }
        } else {
            Color::DarkGray
        };

        let mut title_parts = Vec::new();
        if !p.summary.label.is_empty() {
            title_parts.push(p.summary.label.clone());
        }
        if let Some(tp) = pane_state {
            if let Some(ref proc_name) = tp.foreground_process {
                if title_parts.first().map(|s| s.as_str()) != Some(proc_name.as_str()) {
                    title_parts.push(proc_name.clone());
                }
            } else if let Some(ref t) = tp.title {
                if !t.is_empty() && title_parts.is_empty() {
                    title_parts.push(t.clone());
                }
            }
            if let Some(code) = tp.exit_code {
                title_parts.push(format!("已退出:{}", code));
            }
        }
        if p.is_zoomed {
            title_parts.push("ZOOM".into());
        }
        if scrollback > 0 {
            title_parts.push(format!("回滚 -{} 行 (q 退出)", scrollback));
        }
        if title_parts.is_empty() {
            title_parts.push(short_uuid(&p.block_id));
        }

        let title_str = format!(" {} ", title_parts.join(" · "));
        let block = Block::default()
            .title(Span::styled(
                title_str,
                Style::default()
                    .fg(if p.is_focused {
                        border_color
                    } else {
                        Color::Gray
                    })
                    .add_modifier(if p.is_focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        frame.render_widget(block, p.outer_rect);

        if p.inner_rect.width > 0 && p.inner_rect.height > 0 {
            if p.summary.creator_name == "com.superlogical.terminal" {
                if let Some(tp) = pane_state {
                    render_vt100_screen(tp.parser.screen(), p.inner_rect, frame.buffer_mut());
                    if p.is_focused && !state.show_help {
                        focused_cursor = tp.cursor_screen_pos(p.inner_rect);
                    }
                }
            } else {
                let msg = format!(
                    "非终端类型窗格: {}\n({})",
                    p.summary.creator_name, p.summary.block_id
                );
                frame.render_widget(
                    Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
                    p.inner_rect,
                );
            }
        }
    }

    if state.show_help {
        render_help_overlay(frame, area, true);
    } else if let Some((cx, cy)) = focused_cursor {
        frame.set_cursor_position((cx, cy));
    }
}

fn render_attached_top_bar(
    frame: &mut Frame,
    state: &mut AttachedState,
    host_label: &str,
    transport_kind: &TransportKind,
    area: Rect,
) {
    state.tab_hitboxes.clear();
    let bg_bar = Color::Rgb(24, 28, 36);

    let mut spans: Vec<Span> = Vec::new();
    let mut cur_x: u16 = 0;

    let sess_name = if state.view.label.is_empty() {
        short_uuid(&state.session_id)
    } else {
        state.view.label.clone()
    };
    let badge_text = if *transport_kind == TransportKind::Local {
        format!(" ❐ {} ", sess_name)
    } else {
        format!(" ❐ {} @ {} ", sess_name, host_label)
    };
    cur_x += badge_text.width() as u16;
    spans.push(Span::styled(
        badge_text,
        Style::default()
            .fg(Color::Black)
            .bg(transport_color(transport_kind))
            .add_modifier(Modifier::BOLD),
    ));

    if state.prefix_active {
        let ptxt = " PREFIX (Ctrl-b) ";
        cur_x += ptxt.width() as u16;
        spans.push(Span::styled(
            ptxt,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    } else if state.scroll_mode {
        let stxt = " SCROLL [j/k/PgUp/q] ";
        cur_x += stxt.width() as u16;
        spans.push(Span::styled(
            stxt,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled(" ", Style::default().bg(bg_bar)));
    cur_x += 1;

    let active_wid = state
        .view
        .active_window()
        .map(|w| w.window_id.clone())
        .unwrap_or_default();

    for (idx, win) in state.view.windows.iter().enumerate() {
        let is_active = win.window_id == active_wid;
        let wlabel = if win.label.is_empty() {
            "shell"
        } else {
            win.label.as_str()
        };
        let zoom_mark = if win.zoomed_block_id.is_some() {
            " [Z]"
        } else {
            ""
        };
        let tab_txt = format!(" {}: {}{} ", idx + 1, wlabel, zoom_mark);
        let tab_w = tab_txt.width() as u16;

        state
            .tab_hitboxes
            .push((cur_x, cur_x.saturating_add(tab_w), win.window_id.clone()));
        cur_x = cur_x.saturating_add(tab_w);

        let style = if is_active {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(52, 84, 130))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray).bg(bg_bar)
        };
        spans.push(Span::styled(tab_txt, style));
        spans.push(Span::styled("│", Style::default().fg(Color::DarkGray).bg(bg_bar)));
        cur_x = cur_x.saturating_add(1);
    }

    let hint = " Ctrl-b d: 分离  Ctrl-b ?: 快捷键 ";
    let hint_w = hint.width() as u16;
    if area.width > cur_x + hint_w {
        let pad = (area.width - cur_x - hint_w) as usize;
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg_bar)));
        spans.push(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray).bg(bg_bar),
        ));
    } else if area.width > cur_x {
        let pad = (area.width - cur_x) as usize;
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg_bar)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_help_overlay(frame: &mut Frame, area: Rect, attached: bool) {
    let popup = centered_rect(68, 22, area);
    frame.render_widget(Clear, popup);

    let lines = if attached {
        vec![
            Line::from(Span::styled(
                "连接态快捷键（前缀键 Ctrl-b）：",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  Ctrl-b d          分离当前会话 (Detach)，返回会话管理列表"),
            Line::from("  Ctrl-b n / p      切换到下一个 / 上一个窗口 (Window 标签)"),
            Line::from("  Ctrl-b 1..9       直接切换到第 1..9 个窗口"),
            Line::from("  Ctrl-b 方向键     在当前窗口分屏窗格间按方向切换焦点"),
            Line::from("  Ctrl-b h/j/k/l    在分屏窗格间按左/下/上/右切换焦点"),
            Line::from("  Ctrl-b o / ;      循环切换下一个 / 上一个窗格"),
            Line::from("  Ctrl-b z          最大化 / 还原当前焦点窗格 (Zoom)"),
            Line::from("  Ctrl-b [ / PgUp   进入历史回滚浏览模式 (j/k/PgUp/PgDn, q 退出)"),
            Line::from("  Ctrl-b Ctrl-b     向终端内程序发送原始 Ctrl-b 字符"),
            Line::from("  鼠标左键点击      点击顶部标签切窗口 / 点击分屏切焦点"),
            Line::from(""),
            Line::from(Span::styled(
                "按任意键关闭此帮助面板",
                Style::default().fg(Color::Yellow),
            )),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                "会话与远程主机管理快捷键：",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  Enter             连接 (Attach) 到当前选中的 Rex 会话"),
            Line::from("  j / k 或 ↑ / ↓    上下选择会话，右侧实时预览窗口与分屏结构"),
            Line::from("  n / a             新建会话 / 新建并立即连接"),
            Line::from("  r / d             重命名会话 / 关闭并销毁选中的会话"),
            Line::from("  s 或 H            打开远程主机切换与管理面板（新增/切换/删除）"),
            Line::from("  c                 快速直连临时 HTTP / SSH 地址"),
            Line::from("  [ / ]             在顶部主机栏快速切换上一台 / 下一台主机"),
            Line::from("  R                 手动刷新会话与主机列表"),
            Line::from("  q 或 Esc          退出 rex-tui（后台会话保持运行）"),
            Line::from(""),
            Line::from(Span::styled(
                "支持协议：本机 (Unix Socket / Named Pipe)、http://host:port、ssh://[user@]host[:port]",
                Style::default().fg(Color::Gray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "按任意键关闭此帮助面板",
                Style::default().fg(Color::Yellow),
            )),
        ]
    };

    let block = Block::default()
        .title(" rex-tui 快捷键帮助 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn key_badge(k: &'static str) -> Span<'static> {
    Span::styled(
        format!(" {} ", k),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn short_uuid(full_id: &str) -> String {
    let raw = full_id
        .split_once(':')
        .map(|(_, r)| r)
        .unwrap_or(full_id);
    raw.chars().take(8).collect()
}

fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let w = width.min(r.width.saturating_sub(2)).max(10);
    let h = height.min(r.height.saturating_sub(2)).max(4);
    let x = r.x + (r.width.saturating_sub(w)) / 2;
    let y = r.y + (r.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}
