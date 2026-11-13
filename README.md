# rex-tui

`rex-tui` 是一个基于 **Rust + Ratatui** 开发的 [Rex](https://www.superlogical.com/rex/docs) 终端多路复用客户端（TUI），支持**本机 Unix Socket** 以及**远程机器（HTTP / SSH）**会话管理与类 `tmux` 多窗格实时连接。

## 功能特性 (v0.2)

- **本地与远程多协议连接（支持 Linux / macOS / Windows）**：
  - **本机 (`local` / `unix://` / Windows `Named Pipe`)**：在 Linux/macOS 自动检测本地 Rex Unix Socket（`REX_RUNTIME_DIR`、`XDG_STATE_HOME`、`~/.local/state/rex/run/server.sock` 等），在 Windows 自动连接本地 Rex Named Pipe（`\\.\pipe\...`），未运行时自动通过 `rex server status` 拉起。
  - **远程 HTTP (`http://host[:port]` / `host:port`)**：基于原生 TCP WebSocket 直连局域网或 Tailscale 上的远程 Rex 服务（无需中转进程）。
  - **远程 SSH (`ssh://[user@]host[:port]`)**：复用本机 OpenSSH 配置（`~/.ssh/config`、公钥/`ssh-agent`），自动探测或拉起远端 `rex` 服务，并建立加密转发隧道（Linux/macOS 使用本地 Unix Socket 隧道，Windows 使用本机回环端口直连远端 Unix Socket；断开或切走主机时自动回收隧道进程）。
  - **兼容 `rex hosts` 主机配置**：自动读取 `rex hosts` 保存的远程主机列表，支持多地址按序故障回退（例如优先 Tailscale HTTP，回退 SSH）。
- **多主机管理与无缝切换**：
  - 顶部内置 **主机切换栏 (Host Bar)**，支持鼠标点击、`[` / `]` 键快速轮换主机
  - 支持主机管理弹窗 (`s` / `H`) 新增并持久化主机到 `rex hosts`、删除主机，或按 `c` 快速临时连接任意地址（异步非阻塞连接，支持 `Esc` 随时取消）
- **会话管理面板 (Session Manager)**：
  - 实时列出并监听当前主机的所有 Rex 会话，右侧面板实时展示 **Window（标签）与 Split/Block（分屏窗格）结构树**
  - 支持新建会话 (`n`)、新建并立即连接 (`a` / `N`)、重命名 (`r`)、安全销毁确认 (`d` / `x`)、回车直接连接 (`Enter`)
- **类 tmux 多窗格连接视图 (Attached Multiplexer)**：
  - 顶部渲染当前会话与远程主机徽标（`@ host`）及 **Window 标签栏**，底部按比例渲染平铺分屏 (`tiled`)、浮动层 (`floating`) 与窗格最大化 (`zoom`)
  - 基于 Rex 原生协议 (`rex.control.v1` + `rex.data.terminal.v1` + `vt100`) 实时流式渲染各窗格 ANSI/256 色/TrueColor、光标位置、前景进程名与退出码
  - 支持完整键盘输入顺序合并转发、Bracketed Paste 粘贴保护、鼠标点击标签切窗口/点击分屏切焦点，以及历史回滚浏览模式

## 构建与运行

```bash
# 编译 Release 版本
cargo build --release

# 1. 启动并管理本机会话（同时可在 TUI 内按 s / c 切换到远程主机）
./target/release/rex-tui

# 2. 通过 SSH 直连远程机器（只需远端安装 rex 并配置好 SSH 免密登录）
./target/release/rex-tui -S ssh://user@devbox
./target/release/rex-tui -S ssh://user@10.0.0.8:2222 -s work

# 3. 通过 HTTP 直连局域网 / Tailscale 远程机器
./target/release/rex-tui -S http://100.64.0.12:7500

# 4. 通过 `rex hosts` 已保存的主机别名连接
./target/release/rex-tui -S devbox
```

### 远程机器准备说明

- **SSH 模式（推荐，零额外配置）**：远程机器已安装 `rex`（位于常用 `PATH` 如 `~/.local/bin`、`/usr/local/bin` 等）且本机可通过 SSH 连接即可；在 TUI 内部切换 SSH 主机时建议配置好 SSH 公钥或 `ssh-agent` 免密登录。
- **HTTP 模式（适合内网 / Tailscale）**：在远程机器启动 Rex 时开启 HTTP 监听，例如：
  ```bash
  rex server run --extra-listen http://100.x.y.z:7500 --allow-anonymous
  ```

## 快捷键说明

### 1. 会话管理面板

| 按键 | 功能 |
| --- | --- |
| `Enter` | 连接到当前选中的会话（若无会话则弹出新建窗口） |
| `j` / `k` 或 `↑` / `↓` | 上下移动选择会话 |
| `s` 或 `H` | 打开**主机切换与管理**面板（回车切换、`a` 新增保存主机、`d` 删除、`c` 临时连接） |
| `c` | 快速临时连接指定远程地址（`http://...`、`ssh://...` 或主机别名） |
| `[` / `]` | 快速切换到上一个 / 下一个主机（或鼠标直接点击顶部主机栏） |
| `n` | 在当前主机新建会话 |
| `a` 或 `N` | 在当前主机新建会话并立即进入 |
| `r` | 重命名当前选中的会话 |
| `d` 或 `x` | 关闭并销毁选中的会话（带确认提示） |
| `R` | 手动刷新当前主机的会话列表 |
| `?` | 查看快捷键帮助 |
| `q` 或 `Esc` | 退出 `rex-tui`（连接远程主机进行中按 `Esc` 可取消连接） |

### 2. 连接态（前缀键 `Ctrl-b`）

| 按键 | 功能 |
| --- | --- |
| `Ctrl-b` `d` | **Detach 分离**当前会话，返回当前主机的会话管理面板 |
| `Ctrl-b` `n` / `p` | 切换到下一个 / 上一个 Window 标签 |
| `Ctrl-b` `1`..`9` | 直接跳转到第 1..9 个 Window 标签 |
| `Ctrl-b` `←`/`↓`/`↑`/`→` 或 `h`/`j`/`k`/`l` | 按方向切换焦点分屏窗格 |
| `Ctrl-b` `o` / `;` | 循环切换下一个 / 上一个窗格 |
| `Ctrl-b` `z` | 最大化 / 还原当前窗格 (Zoom) |
| `Ctrl-b` `[` 或 `PgUp` | 进入历史回滚浏览模式（`j`/`k`/`PgUp`/`PgDn` 滚动，`q`/`Esc` 退出） |
| `Ctrl-b` `Ctrl-b` | 向当前终端发送原始 `Ctrl-b` |
| `Ctrl-b` `?` | 打开/关闭快捷键帮助浮层 |
| 鼠标点击 | 点击顶部标签切换 Window；点击分屏区域切换焦点窗格 |
