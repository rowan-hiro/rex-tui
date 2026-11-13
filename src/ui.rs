use crate::app::{App, AppMode, AttachedState, ManagerModal};
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

fn render_session_manager(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    // 1. Header bar
    let pid_info = app
        .server
        .server_pid
        .map(|p| format!("pid {}", p))
        .unwrap_or_else(|| "已连接".to_string());
    let ver_info = app
        .server
        .rex_version
        .as_deref()
        .map(|v| format!(" • rex {}", v))
        .unwrap_or_default();

    let header_line = Line::from(vec![
        Span::styled(
            " REX TUI ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("● 本地服务: ", Style::default().fg(Color::Green)),
        Span::styled(
            &app.server.endpoint_display,
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!(" ({}{})  •  {} 个会话", pid_info, ver_info, app.sessions.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let header = Paragraph::new(header_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(header, chunks[0]);

    // 2. Main body: Left session list + Right live session structure inspector
    let main_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    render_session_list(frame, app, main_cols[0]);
    render_session_inspector(frame, app, main_cols[1]);

    // 3. Footer status + shortcuts
    let mut footer_lines = Vec::new();
    if let Some((ref msg, is_err)) = app.status_msg {
        let color = if is_err { Color::LightRed } else { Color::LightGreen };
        footer_lines.push(Line::from(vec![
            Span::styled(" 提示: ", Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(msg.as_str(), Style::default().fg(color)),
        ]));
    } else {
        footer_lines.push(Line::from(vec![
            key_badge("Enter"),
            Span::raw(" 连接  "),
            key_badge("n"),
            Span::raw(" 新建  "),
            key_badge("a"),
            Span::raw(" 新建并连接  "),
            key_badge("r"),
            Span::raw(" 重命名  "),
            key_badge("d"),
            Span::raw(" 关闭会话  "),
            key_badge("R"),
            Span::raw(" 刷新  "),
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
            key_badge("r"),
            Span::raw(" 重命名  "),
            key_badge("d"),
            Span::raw(" 关闭  "),
            key_badge("?"),
            Span::raw(" 帮助  "),
            key_badge("q"),
            Span::raw(" 退出"),
        ]));
    }
    let footer = Paragraph::new(footer_lines);
    frame.render_widget(footer, chunks[2]);

    // 4. Modal overlays
    match &app.modal {
        ManagerModal::None => {}
        ManagerModal::NewSession {
            input,
            attach_immediately,
        } => {
            let popup = centered_rect(54, 9, area);
            frame.render_widget(Clear, popup);
            let mode_txt = if *attach_immediately {
                "[✓] 创建后立即进入会话 (Tab 切换)"
            } else {
                "[ ] 仅创建会话留在列表 (Tab 切换)"
            };
            let lines = vec![
                Line::from(Span::styled(
                    "输入会话名称（留空则由 Rex 自动命名）：",
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
            let popup = centered_rect(54, 7, area);
            frame.render_widget(Clear, popup);
            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw("  确定要关闭并销毁会话 "),
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
}

fn render_session_list(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(format!(" 本地会话 ({}) ", app.sessions.len()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    if app.sessions.is_empty() {
        let empty_lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "当前暂无运行中的本地 Rex 会话",
                Style::default().fg(Color::Gray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("按 "),
                key_badge("Enter"),
                Span::raw(" 或 "),
                key_badge("n"),
                Span::raw(" 创建第一个会话"),
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
                Span::styled(
                    format!("{:<9}", counts),
                    Style::default().fg(Color::Yellow),
                ),
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

    let AppMode::Attached(ref mut state) = app.mode else {
        return;
    };

    // 1. Render Top Multiplexer Tab Bar & Record Mouse Hitboxes
    render_attached_top_bar(frame, state, top_area);

    // 2. Render Active Window's Split/Floating Panes
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

        // Build informative pane title
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

fn render_attached_top_bar(frame: &mut Frame, state: &mut AttachedState, area: Rect) {
    state.tab_hitboxes.clear();
    let bg_bar = Color::Rgb(24, 28, 36);

    let mut spans: Vec<Span> = Vec::new();
    let mut cur_x: u16 = 0;

    let sess_name = if state.view.label.is_empty() {
        short_uuid(&state.session_id)
    } else {
        state.view.label.clone()
    };
    let badge_text = format!(" ❐ {} ", sess_name);
    cur_x += badge_text.width() as u16;
    spans.push(Span::styled(
        badge_text,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
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
    let popup = centered_rect(64, 20, area);
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
                "会话管理列表快捷键：",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  Enter             连接 (Attach) 到当前选中的 Rex 会话"),
            Line::from("  j / k 或 ↑ / ↓    上下选择会话，右侧实时预览窗口与分屏结构"),
            Line::from("  n                 新建会话"),
            Line::from("  a 或 N            新建会话并立即连接进入"),
            Line::from("  r                 重命名选中的会话"),
            Line::from("  d 或 x            关闭并销毁选中的会话（含确认提示）"),
            Line::from("  R                 手动刷新会话列表"),
            Line::from("  q 或 Esc          退出 rex-tui（后台会话继续保持运行）"),
            Line::from(""),
            Line::from(Span::styled(
                "提示：连接进入会话后，使用 Ctrl-b d 可随时退回此列表。",
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
