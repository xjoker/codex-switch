# codex-switch

[OpenAI Codex CLI](https://github.com/openai/codex) 多账号管理工具，支持配额监控和交互式 TUI 界面。

[**English Documentation →**](README.md)

---

![TUI 截图](docs/tui.png)

## 功能特性

- **账号管理** — 保存、切换、重命名、删除 Codex 账号
- **自动探测** — 自动发现并追踪当前 `auth.json`
- **用量仪表盘** — 实时监控配额（5 小时和 7 天窗口），包含每个账号自己的刷新时间
- **智能切换** — `codex-switch use` 不带参数时通过三种[选择模式](#选择模式)自动选择最优账号（Team 账号优先调度）
- **仅刷新过期账号** — `use`、`list` 和 TUI 默认只刷新缓存已过期的账号
- **进度展示** — 大批量 `use`、`list`、目录 `import` 统一显示单行跨平台进度条
- **交互式 TUI** — 完整的终端界面，实时用量数据、颜色状态、键盘快捷键
- **OAuth 登录** — 内置 PKCE 浏览器登录流程，无需手动复制 token
- **Token 自动刷新** — 使用 refresh_token 自动刷新过期 token
- **批量导入校验** — 支持单文件导入，也支持递归扫描目录、分阶段校验并自动分配不重复别名
- **手动自更新** — `self-update --check` 按需检查 GitHub Releases，`self-update` 更新直装版本
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

### 安装指定版本

```bash
# macOS / Linux
CS_VERSION=0.0.9 curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash

# Windows
$env:CS_VERSION="0.0.9"; irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
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

# 7. 手动检查新版本
codex-switch self-update --check
```

## 命令列表

| 命令 | 说明 |
|------|------|
| `codex-switch use [别名] [-m 模式]` | 切换账号。不带别名则按配置的[选择模式](#选择模式)自动选择最优账号。`-m` 覆盖默认模式 |
| `codex-switch list [-f]` | 显示所有账号信息、用量和可用状态（`-f` 强制刷新，忽略缓存） |
| `codex-switch login [--device] [别名]` | OAuth 登录（`--device` 用于无浏览器的服务器）。若别名已存在则重新授权 |
| `codex-switch rename <旧别名> <新别名>` | 重命名账号 |
| `codex-switch delete <别名>` | 删除账号 |
| `codex-switch import <路径> [别名]` | 导入单个 auth.json，或递归扫描目录下所有 JSON 文件并校验后导入 |
| `codex-switch self-update [--check] [--version <版本>]` | 手动检查 GitHub Releases，或更新当前直装版本 |
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
| `r` | 刷新所有用量数据 |
| `s` | 切换排序（名称/配额/状态） |
| `Space` | 标记/取消标记账号 |
| `b` | 批量刷新已标记账号 |
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

# 更新到指定的新版本
codex-switch self-update --version 0.0.9
```

- Homebrew 安装不会被程序自行覆盖，请使用 `brew upgrade xjoker/tap/codex-switch`
- 直装版本会先校验 release 对应的 `.sha256`，再替换当前二进制
- 不支持降级；`--version` 只接受当前版本或更高版本的 release

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
mode = "max-remaining"      # 选择模式: max-remaining | drain-first | round-robin
min_remaining = 5           # drain-first: 低于此 5h 剩余百分比的账号降级（默认: 5）
safety_margin_7d = 20       # 7d 安全线: 低于此剩余百分比开始扣分（默认: 20）
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
| `~/.codex-switch/config.toml` | 配置文件 |

### 自动探测

运行 `codex-switch list` 或 `codex-switch tui` 时，工具会检查当前 `~/.codex/auth.json` 是否属于未追踪的账号。如果是，自动保存为新 profile（使用邮箱用户名作为别名）。

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

不带别名调用时，`codex-switch use` 会先复用仍然新鲜的缓存，再只刷新过期账号，并对每个账号评分，选择得分最高的进行切换。

算法采用**两阶段**方式：
1. **准入检查** — 已耗尽或 7d 配额严重不足（且重置遥远）的账号被过滤。如果所有账号都不达标，则从中选最优的作为兜底。
2. **评分排名** — 对通过准入的账号按 5h 模式分数 + 7d 健康度调整进行排名。

**Team 账号加权** — 在 `max-remaining` 和 `drain-first` 模式下，Team 计划账号获得 +20 评分加成。在 `round-robin` 模式下，Team 账号被置于更高层级，始终优先于非 Team 账号。

> **注意：** 切换账号后，需要**重启 Codex** 才能加载新的 `auth.json`。Codex CLI 仅在启动时读取认证文件，不会监听文件变化。

### 评分机制

**5h 决定"用谁"，7d 决定"敢不敢用"。**

```
最终分数 = 5h 模式分数 + 7d 调整量   (max-remaining 和 drain-first)
最终分数 = Team 层级 + 最久未使用      (round-robin)
```

对于 `max-remaining` 和 `drain-first`，5h 窗口是主要因素 — 决定当前应该使用哪个账号。7d 窗口作为安全修正：当周配额偏低时，逐步惩罚该账号，但不会完全推翻一个强势的 5h 优势。

对于 `round-robin`，不使用 7d 调整量。取而代之的是准入门槛会过滤掉 7d 严重不足的账号，剩余合格账号按最久未使用的顺序轮换。

#### 7d 健康度调整（max-remaining 和 drain-first）

在 5h 评分后以加法叠加（范围：-300 到 0）：

- **7d 剩余 >= 安全线（默认 20%）** → 不惩罚（安全区）
- **7d 剩余 < 安全线** → 剩余越少惩罚越大
- **7d 在 48 小时内重置** → 惩罚减轻（最多减轻 80%），因为周配额即将恢复
- **7d 重置遥远** → 全额惩罚，因为耗尽意味着数天的锁定
- **无 7d 数据** → 轻微惩罚（-50），未知状态保守处理

最大惩罚 300 分足以在 5h 接近的竞争中翻转结果，但无法推翻较大的 5h 优势 — 确保 5h 始终是主要决策因素。

#### 准入门槛

以下情况账号被标记为**不合格**：
- 任一窗口已完全耗尽（>=100%），或
- 7d 剩余低于临界阈值（安全线的 25%，最低 1%）且 7d 重置超过 48 小时

不合格账号会被排除，除非所有账号都不合格，此时选择得分最高的作为最后手段。

### 选择模式

三种模式控制账号的排名方式。7d 调整应用于 `max-remaining` 和 `drain-first`；`round-robin` 仅通过准入门槛来保护 7d 配额。

| 模式 | CLI 参数 | 说明 |
|------|----------|------|
| `max-remaining` | `-m max-remaining` | **默认。** 选择 5h 剩余配额最多的账号 |
| `drain-first` | `-m drain-first` | 优先选择 5h 即将重置的账号 — 先花"免费"配额 |
| `round-robin` | `-m round-robin` | 在合格账号之间均匀轮换 |

#### `max-remaining`（默认）

**策略：最大化当前可用余量。**

选择 5h 窗口中剩余配额最多的账号。简单直接 — 始终给你能坚持最久的那个账号。

5h 评分：
1. 7d 达 100% → 极低分（账号不可用，分数 0-100）
2. 5h 有剩余配额 → 分数 = `1000 + 剩余百分比`（范围 1000-1100）
3. 5h 耗尽但 7d 未满 → 中等分数，基于 5h 重置倒计时（范围 0-500）
4. 无用量数据 → 中性分数（50）

**适用场景：** 单账号为主，或始终想用"最满"的账号。

#### `drain-first`

**策略：优先消耗即将"免费"的配额，保留不会很快恢复的配额。**

核心思路：如果一个账号的 5h 窗口 30 分钟后就重置，那现在花掉的配额实际上是"免费"的 — 无论如何很快就会恢复。而一个 4 小时后才重置的账号应该留作后备。

5h 评分：
1. 7d 达 100% → 极低分（分数 0-100）
2. 5h 剩余低于 `min_remaining` 阈值（默认 5%）→ 降级（分数 500-600）。配额太少无法支撑完整对话，但仍高于已耗尽的账号
3. 5h 有剩余配额且高于阈值 → 分数 = `1000 + 重置紧迫度加分`（范围 1000-1300）。距重置越近，加分越高
4. 5h 耗尽 → 中等分数，基于重置倒计时（范围 0-500）

**5 个账号示例（7d 均健康）：**

| 账号 | 5h 已用 | 5h 重置 | 5h 分数 | 7d 调整 | 最终 | 原因 |
|------|---------|---------|---------|---------|------|------|
| C | 60% | 20 分钟 | ~1280 | 0 | ~1280 | 有余量 + 即将重置 → 花"免费"配额 |
| D | 40% | 45 分钟 | ~1255 | 0 | ~1255 | 余量充足 + 即将重置 → 第二选择 |
| A | 10% | 4 小时 | ~1060 | 0 | ~1060 | 大量余量但重置遥远 → 留作后备 |
| B | 97% | 10 分钟 | ~597 | 0 | ~597 | 低于阈值 → 降级 |
| E | 100% | 2 小时 | ~380 | 0 | ~380 | 已耗尽 → 等待重置 |

**7d 影响示例：**

| 账号 | 5h 已用 | 5h 重置 | 7d 已用 | 7d 重置 | 5h 分数 | 7d 调整 | 最终 |
|------|---------|---------|---------|---------|---------|---------|------|
| A | 0% | 30 分钟 | 10% | 5 天 | 1300 | 0 | **1300** |
| B | 50% | 2 小时 | 30% | 4 天 | 1180 | 0 | **1180** |
| C | 0% | 30 分钟 | 90% | 12 小时 | 1300 | -60 | **1240** |
| D | 0% | 30 分钟 | 95% | 6 天 | 1300 | -225 | **1075** |

账号 D 虽然 5h 满额，但 7d 仅剩 5% 且 6 天后才重置 — -225 的惩罚让它低于账号 B。

**适用场景：** 3 个以上账号，追求无缝轮换和最大总吞吐量。

#### `round-robin`

**策略：均匀分散使用。**

忽略配额水平（仅判断是否合格）。选择最近最少被 `codex-switch use` 选中的合格账号。Team 账号被置于更高层级，始终优先于非 Team 账号。

评分：
1. 不合格账号（已耗尽或 7d 严重不足且重置遥远）→ 排除
2. Team 账号 → 高层级，层级内按最久未使用排序
3. 非 Team 账号 → 低层级，层级内按最久未使用排序

**适用场景：** 大量账号池，均匀分配比最优配额利用更重要。

### 配置选择模式

**CLI 参数（单次覆盖）：**

```bash
codex-switch use -m drain-first
codex-switch use --mode round-robin
```

**配置文件（持久默认值）：**

```toml
# ~/.codex-switch/config.toml
[use]
mode = "max-remaining"      # max-remaining | drain-first | round-robin
min_remaining = 5           # drain-first: 低于此 5h 剩余百分比的账号降级（默认: 5）
safety_margin_7d = 20       # 7d 安全线: 低于此剩余百分比开始扣分（默认: 20）
```

CLI `-m` 参数始终覆盖配置文件中的设置。

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
