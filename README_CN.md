# codex-switch

**[OpenAI Codex CLI](https://github.com/openai/codex) 多账号管理工具** — 无限账号管理、实时配额监控、智能自动切换。

[**English Documentation →**](README.md)

---

### TUI

![TUI](docs/tui.png)

### CLI

![CLI](docs/cli.png)

## 功能特性

- **账号管理** — 保存、切换、重命名、删除 Codex 账号
- **自动探测** — 自动发现并追踪当前 `auth.json`
- **用量仪表盘** — 实时监控配额（5 小时和 7 天窗口），包含每个账号自己的刷新时间
- **自适应智能切换** — `codex-switch use` 不带参数时通过统一的 5 组件自适应评分算法自动选择最优账号，Team 账号默认优先
- **后台守护进程（Beta）** — 可选的 `daemon` 命令在后台监控当前账号的用量，当超过阈值时自动切换。支持 macOS LaunchAgent 和 Linux systemd 用户服务安装
- **仅刷新过期账号** — `use`、`list` 和 TUI 默认只刷新缓存已过期的账号
- **进度展示** — 大批量 `use`、`list`、目录 `import` 统一显示单行跨平台进度条
- **交互式 TUI** — 完整的终端界面，实时用量数据、颜色状态、键盘快捷键
- **OAuth 登录** — 内置 PKCE 浏览器登录流程，无需手动复制 token
- **Token 自动刷新** — 使用 refresh_token 自动刷新过期 token
- **批量导入校验** — 支持单文件导入，也支持递归扫描目录、分阶段校验并自动分配不重复别名
- **配速标记** — 用量条上显示基于窗口已过时间的预期消耗位置，直观判断用量快慢
- **预热** — `warmup` 发送最小请求以启动配额窗口倒计时，已激活的账号自动跳过
- **手动自更新** — `self-update --check` 按需检查 GitHub Releases，`self-update` 更新直装版本（支持 stable 和 dev 双渠道）
- **启动 Codex** — `launch` 使用指定（或最佳）账号的认证启动 Codex CLI，透传所有参数。认证仅在启动时短暂替换，codex 读取后立即还原，不阻塞其他操作
- **超速预警** — 当用量超过预期配速时，5h/7d 列显示红色 `!` 标记
- **代理支持** — HTTP/HTTPS/SOCKS4/SOCKS5/SOCKS5H，支持鉴权
- **跨平台** — macOS、Linux、Windows（全 RGB 调色板确保 TUI 渲染一致）
- **JSON 输出** — `--json` 参数支持脚本化和自动化

## 安装

### 一键安装（推荐）

**macOS / Linux：**

```bash
curl -fsSL https://raw.githubusercontent.com/xjoker/codex-switch/master/scripts/install.sh | bash
```

**Windows（PowerShell）：**

```powershell
irm https://raw.githubusercontent.com/xjoker/codex-switch/master/scripts/install.ps1 | iex
```

### Homebrew（macOS / Linux）

```bash
brew install xjoker/tap/codex-switch
```

### 安装开发版（最新开发构建）

**macOS / Linux：**

```bash
curl -fsSL https://raw.githubusercontent.com/xjoker/codex-switch/master/scripts/install.sh | bash -s -- --dev
```

**Windows（PowerShell）：**

```powershell
$env:CS_DEV="1"; irm https://raw.githubusercontent.com/xjoker/codex-switch/master/scripts/install.ps1 | iex
```

### 卸载

**macOS / Linux：**

```bash
curl -fsSL https://raw.githubusercontent.com/xjoker/codex-switch/master/scripts/install.sh | bash -s -- --uninstall
```

**Windows（PowerShell）：**

```powershell
$env:CS_UNINSTALL="1"; irm https://raw.githubusercontent.com/xjoker/codex-switch/master/scripts/install.ps1 | iex
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

需要 [Rust](https://rustup.rs/) 1.88+：

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

# 1b. 无浏览器的服务器环境，使用设备码登录：
codex-switch login --device

# 2. 登录另一个账号
codex-switch login

# 3. 查看所有账号及实时用量
codex-switch list

# 4. 切换到指定账号
codex-switch use alice

# 5. 自动切换到最佳可用账号
codex-switch use

# 6. 启动交互式 TUI
codex-switch tui

# 7. 用最佳账号启动 Codex
codex-switch launch

# 8. 用指定账号启动 Codex
codex-switch launch alice -- --model gpt-4o

# 9. 启动后台守护进程（Beta，可选）
codex-switch daemon start

# 10. 手动检查新版本
codex-switch self-update --check
```

## 命令列表

| 命令 | 说明 |
|------|------|
| `codex-switch use [别名] [--force]` | 切换账号。不带别名则用自适应评分算法自动选择最优账号。`--force` 跳过运行中 Codex 进程的警告 |
| `codex-switch list [-f]` | 显示所有账号信息、用量和可用状态（`-f` 强制刷新，忽略缓存） |
| `codex-switch launch [别名] [-- 参数...]` | 用指定账号的认证启动 Codex CLI。不带别名则自适应评分自动选择。`--` 后的参数透传给 codex |
| `codex-switch warmup [别名]` | 发送最小请求以触发 5h/7d 配额窗口倒计时。不带别名则预热所有账号 |
| `codex-switch login [--device] [别名]` | OAuth 登录（`--device` 用于无浏览器的服务器）。若别名已存在则重新授权 |
| `codex-switch rename <旧别名> <新别名>` | 重命名账号 |
| `codex-switch delete <别名>` | 删除账号 |
| `codex-switch import <路径> [别名]` | 导入单个 auth.json，或递归扫描目录下所有 JSON 文件并校验后导入 |
| `codex-switch daemon start [--foreground]` | 启动自动切换守护进程（Beta）。默认后台运行；`--foreground` 用于服务管理器 |
| `codex-switch daemon stop` | 停止运行的 Beta 守护进程 |
| `codex-switch daemon status` | 显示 Beta 守护进程状态 |
| `codex-switch daemon install` | 安装 Beta 守护进程作为系统服务（macOS LaunchAgent / Linux systemd 用户服务） |
| `codex-switch daemon uninstall` | 卸载 Beta 守护进程系统服务 |
| `codex-switch self-update [--check] [--dev]` | 手动检查 GitHub Releases，或更新当前直装版本。`--dev` 切换到开发通道 |
| `codex-switch tui` | 启动交互式终端界面 |
| `codex-switch open` | 在文件管理器中打开配置目录 |

### 全局选项

| 选项 | 说明 |
|------|------|
| `--json` | 以紧凑 JSON 格式输出（适合脚本/管道） |
| `--json-pretty` | 以格式化 JSON 输出 |
| `--proxy <URL>` | 设置代理（参见[代理支持](#代理支持)） |
| `--color <auto\|always\|never>` | 颜色输出模式（默认: auto） |
| `--debug` | 开启调试日志（显示 HTTP 请求、API 响应、缓存状态） |
| `-V, --version` | 打印版本号 |

## TUI 快捷键

| 按键 | 操作 |
|------|------|
| `j` / `k` 或 `↑` / `↓` | 导航 |
| `Enter` | 切换到选中账号 |
| `/` | 搜索/过滤账号 |
| `r` | 刷新用量数据（已标记账号，无标记时刷新全部） |
| `s` | 切换排序（名称/配额/状态） |
| `Space` | 标记/取消标记账号 |
| `w` | 预热账号（已标记账号，无标记时预热全部） |
| `c` | 清除所有标记 |
| `n` | 重命名选中账号 |
| `d` | 删除选中账号（需确认） |
| `q` / `Esc` | 退出 |

## 更新方式

更新检查完全手动触发。`codex-switch` 不会在启动、`list`、`use` 或 TUI 打开时自动检查更新。

```bash
# 检查是否有新版本
codex-switch self-update --check

# 将直装版本更新到最新 release
codex-switch self-update

```

- Homebrew 安装不会被程序自行覆盖，请使用 `brew upgrade xjoker/tap/codex-switch`
- 直装版本会先校验 release 对应的 `.sha256`，再替换当前二进制
- 使用 `--dev` 安装最新开发版，运行 `self-update`（不带 `--dev`）可退回稳定版
- Homebrew 用户需先 `brew uninstall codex-switch` 才能使用 `--dev`

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

[cache]
ttl = 300  # 缓存有效期（秒，默认 300）

[network]
max_concurrent = 20  # 最大并发请求数（默认 20）

[use]
safety_margin_7d = 20       # 7d 安全线：低于此剩余百分比开始惩罚（默认：20）
team_priority = true        # 优先使用 Team 账号，+500 层级加成（默认：true）

[daemon]
poll_interval_secs = 60         # 用量轮询间隔（秒，默认：60）
switch_threshold = 80           # 触发切换的 5h 用量百分比（默认：80）
token_check_interval_secs = 300 # Token 刷新检查间隔（秒，默认：300）
notify = false                  # 切换时桌面通知（默认：false）
log_level = "error"             # 守护进程日志级别（默认："error"）

[launch]
restore_delay_secs = 3          # codex 启动后多少秒还原 auth.json（默认：3）
```

### 示例

```bash
# 命令行参数
codex-switch --proxy socks5h://127.0.0.1:1080 list

# 环境变量
export CS_PROXY="http://user:pass@proxy.corp.com:8080"
codex-switch list

# 标准环境变量（reqwest 自动读取）
export HTTPS_PROXY="http://proxy.corp.com:8080"
codex-switch list
```

## 常见使用场景

### 每次启动 Codex 前自动切换

```bash
# 加入 shell 配置文件（.zshrc / .bashrc）：
codex-switch use && codex
```

### 使用守护进程保持下一次会话就绪（Beta）

当你希望 `codex-switch` 持续监控当前账号并在后台准备好下一次 Codex 启动时，使用 Beta 守护进程：

```bash
# 启动后台守护进程
codex-switch daemon start

# 检查是否在运行
codex-switch daemon status

# 停止守护进程
codex-switch daemon stop

# 安装/卸载用户服务
codex-switch daemon install
codex-switch daemon uninstall
```

Beta 守护进程使用与 `codex-switch use` 相同的自适应评分逻辑。它在每次轮询时刷新当前账号，仅在 `daemon.switch_threshold` 达到或超过阈值且存在更好的候选账号时才切换，并在单独的定时器上刷新即将过期的 Token。守护进程为未来的 Codex 启动做准备；已运行的 Codex 进程在切换后仍需重启。

### 定时刷新 Token（可选）

通过 cron 定时刷新缓存和 Token，让 `codex-switch use` 即时响应：

```bash
# 编辑 crontab
crontab -e

# 每 5 分钟刷新所有账号用量
*/5 * * * * /usr/local/bin/codex-switch list --json > /dev/null 2>&1
```

此任务会定期执行 `codex-switch list`，刷新过期 Token 并缓存用量数据。**不会**自动切换账号。

### CI / 自动化场景

```bash
# 一行命令：切换到最佳账号并启动 Codex
codex-switch use --json && codex --quiet ...
```

## 故障排查

遇到错误时，使用 `--debug` 查看详细的 HTTP 请求、API 响应和缓存状态：

```bash
codex-switch --debug list
codex-switch --debug use
```

如果问题持续存在，请附上 debug 输出（注意脱敏 Token 和邮箱等敏感信息）[提交 Issue](https://github.com/xjoker/codex-switch/issues)。

## 工作原理

### 文件位置

| 路径 | 说明 |
|------|------|
| `~/.codex/auth.json` | Codex CLI 认证文件（或 `$CODEX_HOME/auth.json`） |
| `~/.codex-switch/profiles/<别名>/auth.json` | 保存的账号数据 |
| `~/.codex-switch/current` | 当前激活的账号名 |
| `~/.codex-switch/auth.lock` | 文件锁（序列化 auth.json 切换操作） |
| `~/.codex-switch/config.toml` | 配置文件 |

### 自动探测

每次交互式启动时，codex-switch 会将当前 `~/.codex/auth.json` 与所有已保存的 profile 进行比对：

- **检测到新账号**（例如你运行了 `codex login`）— 提示保存为新 profile
- **已有账号的 Token 已刷新** — 提示更新对应 profile
- **非交互式环境**（管道、cron、CI）— 报告变更但不会静默修改状态

运行 `codex-switch list` 或 `codex-switch tui` 时，工具还会检查当前 `auth.json` 是否属于未追踪的账号，并自动保存为新 profile（使用邮箱用户名作为别名）。

### 去重机制

登录或导入时，工具通过 `account_id`（优先）或 `email`（备选）匹配账号。如果同一账号已以不同别名存在，会更新已有 profile 而非创建重复项。

### 导入校验

`codex-switch import` 会按阶段验证每个候选文件：

1. 文件格式 — 必须是合法 JSON
2. 结构校验 — 必须包含所需 `tokens` 字段，并且 `id_token` 可解码
3. 用量校验 — 调用 token 刷新和 usage 接口确认账号可用（测试可显式跳过）
4. 保存阶段 — 按身份去重，必要时自动分配不冲突别名

如果输入路径是目录，命令会递归扫描所有 `.json` 文件，并分别报告导入成功与跳过原因。

### 智能切换（`codex-switch use`）

不带别名调用时，`codex-switch use` 会先复用仍然新鲜的缓存，再只刷新过期账号，对每个账号使用统一的自适应算法评分。

算法采用**两阶段**方式：
1. **准入检查** — 过滤已耗尽、7d 配额严重不足（且重置遥远）或低于 Free 计划安全底线的账号。如果**所有**账号都不达标，则从中选最优的作为兜底。
2. **自适应评分** — 对通过准入的账号使用五个组件进行排名：

```text
score = tier_bonus + headroom + sustain + drain_value + recency
```

- `tier_bonus`（0 或 +500）— `team_priority = true` 时 Team 账号默认获得优先。这是优先级而非保证：已耗尽或不安全的 Team 账号仍可能落败或被过滤。
- `headroom`（0..1100）— 基于燃烧速率和重置时间的 5h 配速感知剩余容量，而非静态剩余百分比。
- `sustain`（-800..0）— 7d 每窗口预算安全惩罚。
- `drain_value`（0..300）— 对 60 分钟内即将重置的配额给予加分；权重根据池大小和耗尽比率自适应调整。
- `recency`（-60..0）— 轻微分散惩罚，避免反复使用同一账号。

v0.0.13+ 不再有模式选择。此统一算法替代了之前的 `max-remaining`、`drain-first` 和 `round-robin`。

> **注意：** 切换账号后，需要**重启 Codex** 才能加载新的 `auth.json`。Codex CLI 仅在启动时读取认证文件，不会监听文件变化。

#### 准入门槛

以下情况账号被标记为**不合格**：
- 5h 窗口已完全耗尽（>=100%）
- 7d 窗口已完全耗尽（>=100%）
- 7d 剩余低于临界阈值（`safety_margin_7d` 的 25%，最低 1%）且 7d 重置超过 48 小时
- Free 计划账号已低于内置的 5h 安全底线

不合格账号会被排除，除非所有账号都不合格，此时选择得分最高的作为最后手段。

### 配置选项

`[use]` 现在只有两个选项：

- `safety_margin_7d` — 7d 安全线，用于 sustain 组件和准入门槛
- `team_priority` — 默认 `true`；为 Team 账号提供 +500 层级加成

旧版 `mode` 和 `min_remaining` 在 v0.0.13+ 中被忽略并输出警告。

### 缓存行为

- 用量缓存按 profile alias 单独存储在 `~/.codex-switch/cache.json`
- 每条缓存都带自己的刷新时间，JSON 输出会通过 `usage.fetched_at` 暴露出来
- `list`、`use`、TUI 默认只刷新过期账号
- `list -f` 和 TUI 中的 `r` 会强制所有账号绕过缓存
- 目录导入会逐个文件验证，并显示整体进度

### Token 自动刷新

当用量查询返回 HTTP 401/403 时，工具自动尝试使用存储的 `refresh_token` 刷新 token。刷新成功后，新 token 会写回 profile 文件和当前的 auth.json。

### 安全说明

- CLI 和 TUI 都不允许删除当前激活账号
- JSON 模式保证 stdout 只输出机器可读内容，进度和人类提示会走 stderr

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
- **无浏览器的服务器环境：** 使用 `codex-switch login --device` 进行设备码登录 — 会显示一个 URL 和验证码，在任何有浏览器的设备上完成授权即可

### Windows

- 默认 Codex 认证路径：`%USERPROFILE%\.codex\auth.json`
- 浏览器通过 `rundll32.exe url.dll,FileProtocolHandler` 打开
- 文件管理器通过 `explorer.exe` 打开
- 终端：支持 Windows Terminal、PowerShell 和 cmd.exe
- TUI 通过 `crossterm` 使用 Windows Console API 渲染
- **推荐终端：[Windows Terminal](https://aka.ms/terminal)。** Git Bash（mintty）与 TUI 渲染存在已知兼容性问题，请使用 Windows Terminal 或 PowerShell

## JSON 输出

大多数命令都支持 `--json` 机器可读输出（`tui` 和 `open` 除外）：

```bash
# 以 JSON 列出所有账号
codex-switch --json list

# 切换账号并返回结果
codex-switch --json use alice

# JSON 模式检查更新
codex-switch --json self-update --check
```

## 编译

```bash
# Debug 构建
cargo build

# Release 构建（优化并去除符号）
cargo build --release

# 从 macOS 交叉编译 Linux
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl

# 从 macOS/Linux 交叉编译 Windows
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## 更新日志

每个版本的详细变更记录请参见 [docs/CHANGELOG.md](docs/CHANGELOG.md)。

## 许可证

[MIT](LICENSE)
