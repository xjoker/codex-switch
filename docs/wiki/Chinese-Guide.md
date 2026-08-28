# 中文指南

> 英文 Wiki 是 `codex-switch` 的主文档与行为依据。本页提供中文快速入口与常用操作摘要；细节、标志位与边界条件以英文页面为准（尤其 [Providers](Providers)、[Command reference](Command-Reference)）。

`codex-switch` 用于管理本机多个 OpenAI Codex CLI 登录、查看额度，并在新会话前选择合适账号。它也会保存自定义 API 提供方（如 OpenRouter），通过 `launch` 把端点与密钥交给 Codex，而不改写 `~/.codex`。请勿分享 profile、`auth.json`、提供方 API 密钥、代理凭据或未脱敏的 debug 输出。

## 快速开始

Codex 必须使用 file credential store。在 `$CODEX_HOME/config.toml`（通常是 `~/.codex/config.toml`）中确认：

```toml
cli_auth_credentials_store = "file"
```

macOS / Linux 安装正式版：

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash
```

如果未传 `--system`，脚本却显示 `Installing to /usr/local/bin (requires sudo)`，说明运行的是旧 `master` 分支中的已淘汰脚本，请终止并改用上面的 Release 地址。当前脚本默认安装到 `~/.local/bin`；只有清理 `/usr/local/bin` 中由 root 持有的旧二进制时，才会请求一次 `sudo`。

Windows PowerShell 安装正式版：

```powershell
irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
```

添加 ChatGPT 账号并打开界面：

```bash
codex-switch login work    # 别名可省略，之后可 rename
codex-switch tui
```

无浏览器服务器使用 `codex-switch login --device`。

已有 `auth.json` 备份可导入：

```bash
codex-switch import ~/auth-backups
```

## 日常操作（ChatGPT 账号）

| 目的 | 命令 / 操作 |
|---|---|
| 查看额度与状态 | `codex-switch list`；强制刷新加 `-f` |
| 自动选最佳账号 | `codex-switch use` |
| 切换到指定账号 | `codex-switch use <别名>` |
| 用某账号启动 Codex（结束后恢复现场 `auth.json`） | `codex-switch launch <别名> -- [codex 参数]` |
| 自动选号并启动 | `codex-switch launch -- [codex 参数]` |
| 重命名 / 删除（非当前） | `codex-switch rename` / `delete`（删除可恢复，见 [故障排查](Troubleshooting)） |
| 脚本输出 JSON | 加 `--json` 或 `--json-pretty` |

要点：

- `use` 只切换 ChatGPT 的 `$CODEX_HOME/auth.json`；**不能**用于自定义提供方。
- 已在跑的 Codex 进程不会自动换号，需重启 Codex，或用 `launch` 开新进程。
- Codex 参数写在 `--` 后面：`codex-switch launch work -- exec --json "…"`。`exec` / `resume` 等 Codex 子命令也可以直接跟在 `launch` 后面，不必再写 `--`。`--` 两侧的参数都会保留。prompt 看起来像别名时仍须 `--`。
- 当前 Codex 没有 `--full-auto`；用 `-a never`、`--sandbox` 或 `--dangerously-bypass-approvals-and-sandbox`。
- 池子耗尽时，交互式 `use` / `launch` 可提示消耗重置卡；脚本须显式加 `--consume-card`。

数据默认在 `~/.codex-switch`（可用 `CODEX_SWITCH_HOME` 迁移）；活号在 `~/.codex/auth.json`（可用 `CODEX_HOME` 迁移）。

## 自定义 API 提供方

一个提供方 = **一个端点 URL + 一把 API 密钥 + 多个模型**。别名（Alias）是唯一对用户可见的名称；思考等级（reasoning）与 `web_search` 按**模型**保存，不是按整个提供方。

### CLI

```bash
# 添加（第一个 --model 为默认模型；--reasoning / --no-web-search 作用于最近一个 --model）
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex \
  --model deepseek/deepseek-r1-0528 --reasoning medium

# 小网关也可从 GET /models 拉对话模型（embedding / reranker 会去掉；超过 48 条用 --model 勾选，或 TUI `f`）
printf '%s' "$KEY" | codex-switch provider add zai \
  --base-url https://api.example/v1 \
  --fetch-models \
  --api-key-stdin
codex-switch provider fetch-models zai
# OpenRouter 这类大目录：
codex-switch provider fetch-models openrouter --model openai/gpt-4.1-nano

# 查看 / 改名 / 删除
codex-switch provider list
codex-switch provider show openrouter
codex-switch provider rename openrouter orouter
codex-switch provider remove openrouter    # 非交互须加 --yes

# 启动（不写 ~/.codex；`--model` 在 `--` 前须是已保存的模型 id）
codex-switch launch openrouter
codex-switch launch openrouter --model deepseek/deepseek-r1-0528
codex-switch launch openrouter -- exec --json "review this"
codex-switch launch openrouter -- -s workspace-write -a never
```

密钥约定：

- **永远不要**把 API 密钥写在命令行参数里。
- `provider add` 用隐藏输入读取密钥；脚本用 `--api-key-stdin` 从标准输入读。
- 密钥存在 `$CODEX_SWITCH_HOME/providers/<别名>/provider.toml`（目录 `0700`，文件 `0600`），`list` / `show` / JSON / TUI 只显示打码形式（`…` + 末四位）。
- `launch` 时密钥只注入 Codex 子进程环境变量（默认 `CODEX_SWITCH_<别名>_KEY`），不出现在进程 argv。

限制：

- Codex 目前只支持 `wire_api = "responses"`；DeepSeek 官方 Chat Completions API 不能直连，须走 OpenRouter 等网关。
- `use` 与无别名的 `launch` 自动选号**仅面向 ChatGPT**，不会自动选提供方。
- 提供方别名不能与 ChatGPT profile、其他提供方或 Codex 保留 id（`openai` / `ollama` / `lmstudio`）冲突。
- 删除提供方**不可恢复**（不像 ChatGPT profile 会进 `deleted-profiles/`）。

完整说明见英文 [Custom API providers](Providers)。

## TUI 操作说明

运行 `codex-switch tui`。三页：**Accounts**（ChatGPT 额度与选号）、**Providers**（自定义提供方）与 **Settings**（编辑 `config.toml`）。`Tab` / `Shift+Tab` 循环切换；`h` 帮助；`q` 退出。TUI 内按 `h` 看到的快捷键表与代码同源，以当前版本为准。

设置 `NO_COLOR` 时，**CLI** 仍遵守无颜色；**TUI** 仍使用设计好的深色配色，避免浅色终端把按键提示洗成黑字。

### Accounts 页

| 键 | 作用 |
|---|---|
| `j` / `k` 或方向键 | 移动选中行 |
| `Enter` | 打开账号菜单；若已勾选多账号则打开批量菜单 |
| `/` | 过滤账号 |
| `r` | 刷新当前可见账号 |
| `a` | 添加账号 |
| `o` | 用选中账号启动 Codex |
| `Space` | 勾选 / 取消勾选（批量操作） |
| `t` | 开关自动刷新 |
| `W` | 开关自动预热（5h 窗口过期时） |
| `i` | 显示 / 隐藏紧凑额度面板 |
| `s` | 循环排序（名称 / 额度 / 状态） |
| `Esc` | 清除过滤、勾选或关闭弹层 |

在账号菜单内：`u` 切换、`o` 启动、`w` 预热、`l` **重新登录**、`c` 消耗最早过期的重置卡、`n` 改名、`d` 删除（均需确认）。批量菜单内 `r` / `w` / `l` / `d` 作用于已勾选账号。

### Providers 页

| 键 | 作用 |
|---|---|
| `j` / `k` 或方向键 | 移动选中行 |
| `a` | 新增提供方（表单） |
| `Enter` / `o` | 启动：先选已保存模型，可改本次 reasoning，再启动 Codex |
| `e` | 编辑选中提供方 |
| `n` | 改名 |
| `d` | 删除（需确认） |
| `Tab` | 下一页（Settings） |

**`l` 在 Providers 页不是启动**；启动用 `o` 或 `Enter`。`l` 只在 Accounts 页表示重新登录。

列表不显示完整密钥。

### Settings 页

编辑 `$CODEX_SWITCH_HOME/config.toml`（含 `daemon.auto_warmup`、`warmup_times`、`timezone`）。`j` / `k` 移动字段，`Enter` 编辑或开关，`s` 保存。Accounts 页的 `s` 仍是排序。TUI 的 `W` 只是本次会话开关，不写 `auto_warmup`。`timezone` 留空则用系统时区，也可填 IANA 名称（如 `Asia/Shanghai`）。`warmup_times` 最多 10 个，可一次粘贴多个 `HH:MM`（逗号或空格分隔）；间隔不限制。加完仍停在新增行。保存会重写整个配置文件，不保留注释。未保存的修改切走 Tab 仍会保留；正在编辑字段时 `Tab` 不会切页，`Esc` 取消当前编辑。守护进程的轮询/Token/缓存间隔需重启后生效；`warmup_times` 与 `timezone` 约每分钟重读一次。详情以英文 [Configuration](Configuration) 为准。

### 提供方表单（新增 / 编辑）

新增与编辑共用一张表单：

- **新增**：打开后直接输入 Alias；`Enter` 提交当前字段并进入下一项（Alias → URL → Key → Models；env key / wire API / extra `-c` 保持默认）。
- **编辑**：从 Base URL 的导航态开始（避免 `s` 被当成输入字符）；`Enter` 进入当前格编辑。
- `Tab` 走遍每一栏，包括 Env key、Wire API、Extra `-c`；在 Models 内用 `j` / `k` 移动。模型很多时表头和底栏帮助钉住，只滚动模型视口并跟着光标；超出一屏时标题显示 `n/N`。 Extra `-c` 是 `KEY=VALUE`，值里的逗号会保留。
- 模型列表最后一行是 **`+ add model`**：`Enter` 或 `+` / `=` / `a` 添加模型并输入 id。导航态按 `f` 从接入站 `GET /models` 拉取对话模型（去掉 embedding / reranker；超过 48 条打开选择器：`/` 过滤，`space` 勾选，`Enter` 应用）。
- `←` / `→` 切换该模型的 reasoning；`w` 开关 `web_search`；`*` 标为默认模型。
- `d` / `-` / `Delete` 删除模型前会弹出确认（`y` 删除，`n` 或 `Esc` 取消且**不关闭整张表单**）。至少保留一个模型，**最后一条不能删**。
- 编辑时 API Key 留空表示**保留原密钥**。
- `s` 保存；`Esc` 取消整张表单。
- 改名在列表按 `n`，表单里没有第二个「显示名」字段。

启动选择器（Providers 上 `Enter` / `o`）：`←` / `→` 只改**本次会话**的 reasoning，不写回配置文件。选 `(skip)` 时会清掉该提供方隔离 `CODEX_HOME` 里上次留下的思考等级，避免 Codex 0.150 仍显示 `high` 并向网关带上 `reasoning.effort`。`Tab` 编辑本次额外的 Codex argv（空白拆分）。

Codex 在前台运行；退出后回到 TUI。

## 参与开发版测试

开发版属于滚动 prerelease 通道。安装、验证、回退和问题反馈步骤见 [Testing development releases](Development-Releases)，其中附有中文摘要。

当前滚动开发版示例：`codex-switch self-update --dev`，版本号形如 `20260828.1.0-dev`。

## 常用入口（英文正文）

- [开始使用](Getting-Started) — 安装、登录和首次启动
- [功能指南](Feature-Guide) — 主要工作流与安全边界
- [自定义 API 提供方](Providers) — CLI、存储、`provider.toml`、OpenRouter / DeepSeek 经网关
- [命令参考](Command-Reference) — 全部命令、全局选项与完整 TUI 表
- [配置](Configuration) — 路径、代理、daemon 与 launch 设置
- [更新](Updating) — 更新方式、通道切换和旧版本迁移
- [故障排查](Troubleshooting) — 常见错误与恢复方式
- [常见问题](FAQ) — 简短问答

命令行为以已安装版本的 `codex-switch <命令> --help` 为最终依据。

## 反馈问题

提交 Issue 时请附操作系统、终端、`codex-switch --version`、完整命令、预期结果、实际结果与最小复现步骤。分享 debug 输出前必须删除 Token、提供方密钥、邮箱、account ID、工作区名称、可识别身份的路径和代理凭据。

[提交 GitHub Issue](https://github.com/xjoker/codex-switch/issues)

## Next steps

- 第一次使用：继续阅读[开始使用](Getting-Started)。
- 日常操作与 daemon：查看[功能指南](Feature-Guide)。
- 提供方与模型报错：先看英文 [Providers](Providers) 与 [故障排查](Troubleshooting)。
