# rex-tui

`rex-tui` 是一个基于 **Rust + Ratatui** 开发的本地 [Rex](https://www.superlogical.com/rex/docs) 终端多路复用客户端（TUI），提供本地会话管理与类 `tmux` 多窗格实时连接体验。

## 功能特性 (v0.1)

- **本地服务自动发现与拉起**：自动检测本地 Rex Unix Socket（支持 `REX_RUNTIME_DIR`、`XDG_STATE_HOME`、`~/.local/state/rex/run/server.sock`），若未运行则通过 `rex server status` 自动启动。
- **会话管理面板 (Session Manager)**：
  - 实时列出并监听本地所有 Rex 会话（自动同步外部新建、重命名、关闭事件）
  - 右侧面板实时展示当前选中会话的 **Window（标签）与 Split/Block（分屏窗格）结构树**
  - 支持新建会话 (`n`)、新建并立即连接 (`a` / `N`)、重命名 (`r`)、安全销毁确认 (`d` / `x`)、回车直接连接 (`Enter`)
- **类 tmux 多窗格连接视图 (Attached Multiplexer)**：
  - 顶部渲染当前会话徽标与 **Window 标签栏**，底部区域实时按比例渲染平铺分屏 (`tiled`)、浮动层 (`floating`) 与窗格最大化 (`zoom`)
  - 基于 Rex 原生协议 (`rex.control.v1` + `rex.data.terminal.v1` + `vt100`) 实时流式渲染各窗格 ANSI/256 色/TrueColor、光标位置、前景进程名与退出码
  - 支持完整键盘输入转发、Bracketed Paste 粘贴保护、鼠标点击标签切窗口/点击分屏切焦点，以及历史回滚浏览模式

## 构建与运行

```bash
# 编译 Release 版本
~/.cargo/bin/cargo build --release

# 启动会话管理面板
./target/release/rex-tui

# 直接按名称或 ID 连接到指定会话
./target/release/rex-tui -s work
```

## 快捷键说明

### 1. 会话管理面板

| 按键 | 功能 |
| --- | --- |
| `Enter` | 连接到当前选中的会话（若无会话则弹出新建窗口） |
| `j` / `k` 或 `↑` / `↓` | 上下移动选择会话 |
| `n` | 新建会话 |
| `a` 或 `N` | 新建会话并立即进入 |
| `r` | 重命名当前选中的会话 |
| `d` 或 `x` | 关闭并销毁选中的会话（带确认提示） |
| `R` | 手动刷新会话列表 |
| `?` | 查看快捷键帮助 |
| `q` 或 `Esc` | 退出 `rex-tui`（后台会话继续运行） |

### 2. 连接态（前缀键 `Ctrl-b`）

| 按键 | 功能 |
| --- | --- |
| `Ctrl-b` `d` | **Detach 分离**当前会话，返回会话管理面板 |
| `Ctrl-b` `n` / `p` | 切换到下一个 / 上一个 Window 标签 |
| `Ctrl-b` `1`..`9` | 直接跳转到第 1..9 个 Window 标签 |
| `Ctrl-b` `←`/`↓`/`↑`/`→` 或 `h`/`j`/`k`/`l` | 按方向切换焦点分屏窗格 |
| `Ctrl-b` `o` / `;` | 循环切换下一个 / 上一个窗格 |
| `Ctrl-b` `z` | 最大化 / 还原当前窗格 (Zoom) |
| `Ctrl-b` `[` 或 `PgUp` | 进入历史回滚浏览模式（`j`/`k`/`PgUp`/`PgDn` 滚动，`q`/`Esc` 退出） |
| `Ctrl-b` `Ctrl-b` | 向当前终端发送原始 `Ctrl-b` |
| `Ctrl-b` `?` | 打开/关闭快捷键帮助浮层 |
| 鼠标点击 | 点击顶部标签切换 Window；点击分屏区域切换焦点窗格 |
