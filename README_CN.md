# codex-switch

[OpenAI Codex CLI](https://github.com/openai/codex) 多账号管理工具，支持配额监控和交互式 TUI 界面。

[**English Documentation →**](README.md)

---

![TUI 截图](docs/tui.png)

## 功能特性

- **账号管理** — 保存、切换、重命名、删除 Codex 账号
- **自动探测** — 自动发现并追踪当前 `auth.json`
- **用量仪表盘** — 实时监控配额（5 小时和 7 天窗口），状态指示
- **智能切换** — `codex-switch use` 不带参数时自动选择剩余配额最多的账号
- **交互式 TUI** — 完整的终端界面，实时用量数据、颜色状态、键盘快捷键
- **OAuth 登录** — 内置 PKCE 浏览器登录流程，无需手动复制 token
- **Token 自动刷新** — 使用 refresh_token 自动刷新过期 token
- **代理支持** — HTTP/HTTPS/SOCKS4/SOCKS5/SOCKS5H，支持鉴权
- **跨平台** — macOS、Linux、Windows
- **JSON 输出** — `--json` 参数支持脚本化和自动化

## 安装

### 一键安装（推荐）

**macOS / Linux：**

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash
```

**Windows（PowerShell）：**

```powershell
irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
```

### Homebrew（macOS / Linux）

```bash
brew install xjoker/tap/codex-switch
```

### 手动下载

从 [Releases](https://github.com/xjoker/codex-switch/releases) 下载对应平台的预编译二进制：

| 平台 | 架构 | 文件 |
|------|------|------|
| macOS | Apple Silicon (M1/M2/M3) | `cs-darwin-arm64.tar.gz` |
| macOS | Intel | `cs-darwin-amd64.tar.gz` |
| Linux | x86_64 | `cs-linux-amd64.tar.gz` |
| Linux | ARM64 | `cs-linux-arm64.tar.gz` |
| Windows | x86_64 | `cs-windows-amd64.zip` |
| Windows | ARM64 | `cs-windows-arm64.zip` |

### 从源码编译

需要 [Rust](https://rustup.rs/) 1.85+：

```bash
git clone https://github.com/xjoker/codex-switch.git
cd codex-switch
cargo build --release
sudo cp target/release/codex-switch /usr/local/bin/  # macOS/Linux
```

## 快速开始

```bash
# 1. 登录第一个 Codex 账号
codex-switch login

# 2. 登录另一个账号
codex-switch login

# 3. 查看所有账号及实时用量
codex-switch status

# 4. 切换到指定账号
codex-switch use alice

# 5. 自动切换到最佳可用账号
codex-switch use

# 6. 启动交互式 TUI
codex-switch tui
```

## 命令列表

| 命令 | 说明 |
|------|------|
| `codex-switch use [别名]` | 切换账号。不带别名则自动选择配额最优的账号 |
| `codex-switch list` | 列出所有已保存的账号（自动探测当前 auth.json） |
| `codex-switch status` | 显示所有账号信息、用量和可用状态 |
| `codex-switch login [别名]` | OAuth 登录。若别名已存在则重新授权该账号 |
| `codex-switch delete <别名>` | 删除账号 |
| `codex-switch import <文件> <别名>` | 导入 auth.json 文件为账号 |
| `codex-switch tui` | 启动交互式终端界面 |
| `codex-switch open` | 在文件管理器中打开配置目录 |

### 全局选项

| 选项 | 说明 |
|------|------|
| `--json` | 以 JSON 格式输出（适合脚本/管道） |
| `--proxy <URL>` | 设置代理（参见[代理支持](#代理支持)） |
| `--color <auto\|always\|never>` | 颜色输出模式（默认: auto） |

## TUI 快捷键

| 按键 | 操作 |
|------|------|
| `j` / `k` 或 `↑` / `↓` | 导航 |
| `Enter` | 切换到选中账号 |
| `r` | 刷新所有用量数据 |
| `n` | 重命名选中账号 |
| `d` | 删除选中账号（需确认） |
| `q` / `Esc` | 退出 |

## 代理支持

代理优先级（从高到低）：

1. `--proxy` 命令行参数
2. `CS_PROXY` 环境变量
3. 配置文件 `~/.codex-switch/config.toml`
4. 标准环境变量（`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`）

### 支持的协议

| 协议 | DNS 解析 | 鉴权 |
|------|----------|------|
| `http://[user:pass@]host:port` | 本地 | 支持 |
| `https://[user:pass@]host:port` | 本地 | 支持 |
| `socks4://host:port` | 本地 | 不支持 |
| `socks5://[user:pass@]host:port` | 本地 | 支持 |
| `socks5h://[user:pass@]host:port` | 远程（代理端解析） | 支持 |

### 配置文件

`~/.codex-switch/config.toml`：

```toml
[proxy]
url = "socks5h://user:pass@127.0.0.1:1080"
no_proxy = "localhost,127.0.0.1"
```

### 示例

```bash
# 命令行参数
codex-switch --proxy socks5h://127.0.0.1:1080 status

# 环境变量
export CS_PROXY="http://user:pass@proxy.corp.com:8080"
codex-switch status
```

## 工作原理

### 文件位置

| 路径 | 说明 |
|------|------|
| `~/.codex/auth.json` | Codex CLI 认证文件（或 `$CODEX_HOME/auth.json`） |
| `~/.codex-switch/profiles/<别名>/auth.json` | 保存的账号数据 |
| `~/.codex-switch/current` | 当前激活的账号名 |
| `~/.codex-switch/config.toml` | 配置文件 |

### 自动探测

运行 `codex-switch list`、`codex-switch status` 或 `codex-switch tui` 时，工具会检查当前 `~/.codex/auth.json` 是否属于未追踪的账号。如果是，自动保存为新 profile（使用邮箱用户名作为别名）。

### 去重机制

登录或导入时，工具通过 `account_id`（优先）或 `email`（备选）匹配账号。如果同一账号已以不同别名存在，会更新已有 profile 而非创建重复项。

### 智能切换（`codex-switch use`）

不带别名调用时，`codex-switch use` 并发查询所有账号并评分：

1. 7 天（周）限额达 100% → 极低分（账号不可用）
2. 5 小时窗口有剩余配额 → 高分（1000 + 剩余百分比）
3. 5 小时窗口耗尽但周限额未满 → 中等分数（根据重置时间）

选择得分最高的账号并切换。

### Token 自动刷新

当用量查询返回 HTTP 401/403 时，工具自动尝试使用存储的 `refresh_token` 刷新 token。刷新成功后，新 token 会写回 profile 文件和当前的 auth.json。

## 平台说明

### macOS

- 默认 Codex 认证路径：`~/.codex/auth.json`
- 浏览器通过系统 `open` 命令打开
- 文件管理器通过 `open` 打开

### Linux

- 默认 Codex 认证路径：`~/.codex/auth.json`
- 浏览器通过 `xdg-open` 打开（确保已配置桌面浏览器）
- 文件管理器通过 `xdg-open` 打开
- WSL：浏览器打开可能需要 `wslu` 包（`sudo apt install wslu`）

### Windows

- 默认 Codex 认证路径：`%USERPROFILE%\.codex\auth.json`
- 浏览器通过 `rundll32.exe url.dll,FileProtocolHandler` 打开
- 文件管理器通过 `explorer.exe` 打开
- 终端：支持 Windows Terminal、PowerShell 和 cmd.exe
- TUI 通过 `crossterm` 使用 Windows Console API 渲染

## 许可证

[MIT](LICENSE)
