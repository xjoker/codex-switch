# 中文指南

> 英文 Wiki 是 `codex-switch` 的主文档与行为依据。本页提供中文快速入口与常用操作摘要；细节、标志位与边界条件以英文页面为准（尤其 [Providers](Providers)、[Command reference](Command-Reference)）。

`codex-switch` 用于管理本机多个 OpenAI Codex CLI 登录、查看额度，并在新会话前选择合适账号。它也提供 **Beta 自定义 API 提供方**：保存兼容 Responses 协议的端点和 API 密钥，为多个模型分别设置思考等级与 `web_search`，获取网关模型目录并启动 Codex。兼容性取决于网关和具体模型；提供方不展示 ChatGPT 额度，也不参与自动选号。提供方沿用原来的 `$CODEX_HOME`，直接使用现有 MCP、skills、agents、插件和 hooks；每次启动选择独立的 Codex 原生 profile，模型与路由设置不会写入默认 `config.toml`。支持多个提供方同时运行，启动工具异常退出不需要恢复默认配置。请勿分享 profile、`auth.json`、提供方 API 密钥、代理凭据或未脱敏的 debug 输出。

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

ChatGPT 文件登录需要 `cli_auth_credentials_store = "file"`。登录、导入、切换及对应后端请求会预检有效的系统/MDM 认证策略；普通配置不能放宽管理员的登录方式、工作区或存储要求。存在 `OPENAI_FEDERATION_RULE_ID` 或 `OPENAI_IDENTITY_TOKEN_FILE`（即使为空）时，Codex 会优先选择工作负载身份联合认证，因此文件登录操作会被拒绝；工具不会删除这些变量或改写企业策略。codex-switch 自身发起的 ChatGPT 请求（用量、刷新、工作区元数据、预热、模型、重置卡）不支持自定义 `chatgpt_base_url`，会在请求前明确报错；`use`、`import`、`login`、`launch` 等纯本地命令不受其影响。API 提供方使用独立的密钥启动路径。

`codex-switch launch` 的项目支持基线是 Codex CLI 0.159.2 或更新版本。先用 `codex-switch doctor` 检查 PATH 中实际解析到的 CLI；这是本项目的支持基线，不表示旧版 Codex 一定无法独立工作。桌面版可能使用另一个引擎，可用 `codex-switch doctor --desktop-codex <桌面版内置引擎路径>` 单独检查，不会自动搜索桌面安装。Windows 下 WSL 默认使用独立 Linux home，不会自动共享 Windows Codex app 的配置、认证和会话；参见 OpenAI 的 [Windows app 与 WSL 说明](https://learn.chatgpt.com/docs/windows/windows-app)。

已有 `auth.json` 备份可导入：

```bash
codex-switch import ~/auth-backups
```

## 日常操作（ChatGPT 账号）

| 目的 | 命令 / 操作 |
|---|---|
| 查看额度与状态 | `codex-switch list`；强制刷新加 `-f` |
| 一次性预热 5h 额度窗口 | `codex-switch warmup` 或 `codex-switch warmup <别名>`（只有 7d 窗口的账号会跳过） |
| 自动选最佳账号 | `codex-switch use` |
| 切换到指定账号 | `codex-switch use <别名>` |
| 用某账号启动 Codex（结束后恢复现场 `auth.json`） | `codex-switch launch <别名> -- [codex 参数]` |
| 自动选号并启动 | `codex-switch launch -- [codex 参数]` |
| 重命名 / 删除（非当前） | `codex-switch rename` / `delete`（删除可恢复，见 [故障排查](Troubleshooting)） |
| 脚本输出 JSON | 加 `--json` 或 `--json-pretty` |

要点：

- `use` 和 Accounts 页的 `u` 切换 ChatGPT 的 `$CODEX_HOME/auth.json`，并将用户 `config.toml` 顶层及其默认 `profile` 中已有的 `model_provider` 选择改回 `openai`；保留模型、提供方定义、MCP 和注释。**不能**用提供方别名执行 `use`。
- 若另行启动仍请求第三方地址，检查启动参数、项目 `.codex/config.toml`、额外指定的 Codex profile，以及 `openai_base_url` / `OPENAI_BASE_URL` 地址覆盖；这些不由账号切换修改。
- `use` 不会后台自动换号。Codex 0.157 起交互会话挂在共享的 app-server daemon 上，它只在启动时读一次 `auth.json`，所以 daemon 在运行时 `use` / `login` 会自动执行 `codex app-server daemon restart`（挂在上面的会话会重连到新账号，进行中的回合会被打断；每次 daemon 调用最多等 15 秒，超时按重启失败提示。不想被打断可在 `config.toml` 设 `[use] restart_app_server = false` 或在 TUI 设置页关闭，此时只提示手动命令）；`codex exec` 或 `--no-daemon` 的进程不会读取新的 `auth.json`，需重启 Codex，或用 `launch` 开新进程（Codex 的 `--help` 列出 `--no-daemon` 时，ChatGPT 的 launch 会自动加上；已带 `--no-daemon`、`--remote` 或 `agents` 的参数原样传递；这次 `--help` 探测最多等 10 秒，若无法判断路由，launch 会在写入任何凭据之前直接拒绝）。判断 `auth.json` 是否变化时比较的是规范化后的 JSON，仅格式变化（例如 Codex 自己刷新 Token 后重写文件）或重新选中已在用的账号都不会重启 daemon；Windows 上目录 ACL 已经是加固状态时不再重复写入，所以切换很快。
- `launch` 会先对同一个已解析的 PATH Codex CLI 执行最低版本检查；明确读到低于 0.159.2 的版本会在选择账号、消耗重置卡、创建 provider run 或写入凭据前拒绝启动（0.159.2 的 prerelease，如 `0.159.2-rc.1`，视为满足）；版本未知或探测失败（超时、报错、无法解析）只在 stderr 给出警告并继续启动。10 秒版本探测使用临时 `CODEX_HOME`；原有 `codex --help` 能力探测仍单独保留。版本 0.159.2 是本项目支持基线，不是断言更旧 Codex 无法独立工作。
- Codex 参数写在 `--` 后面：`codex-switch launch work -- exec --json "…"`。`exec` / `resume` 等 Codex 子命令也可以直接跟在 `launch` 后面，不必再写 `--`。`--` 两侧的参数都会保留。prompt 看起来像别名时仍须 `--`。
- 当前 Codex 没有 `--full-auto`；用 `-a never`、`--sandbox` 或 `--dangerously-bypass-approvals-and-sandbox`。
- 池子耗尽时，交互式 `use` / `launch` 可提示消耗重置卡；脚本须显式加 `--consume-card`。
- `warmup` 读完整个响应流并要求出现 `response.completed` 才算成功；选模型顺序为 luna、mini、再按 API 优先级；遇到 HTTP 400 “not supported” 时，唯一一次重试会排除被拒绝的模型；预热前主动刷新 Token 若只是暂时失败（如网络错误），之后遇到 401 仍保留一次恢复性刷新。
- Token 刷新后的身份校验只拒绝真正换了账号的情况：声明从缺失变为存在，或同一 `account_id` 下邮箱改变，都会被接受并保存。

数据默认在 `~/.codex-switch`（可用 `CODEX_SWITCH_HOME` 迁移）；活号在 `~/.codex/auth.json`（可用 `CODEX_HOME` 迁移）。

如果旧版本安装过 daemon：旧的 LaunchAgent、systemd 用户单元或 Windows 计划任务（以及旧版 `self-update` 的 daemon 重启）启动新二进制时，新版本会删除这条旧注册并退出，不会再反复启动失败。也可以手动运行 `codex-switch daemon uninstall` 完成清理；清理失败时按提示的命令处理。完整迁移说明见 [Updating](Updating#migrate-from-the-removed-daemon)。

## 自定义 API 提供方（Beta）

检查 PATH CLI 以及可选的桌面内置引擎：

```bash
codex-switch doctor
codex-switch doctor --desktop-codex <桌面版内置引擎路径>
```

未提供桌面路径会报告 `not_checked`，不会失败。未知/低于最低版本会失败并输出诊断；两个引擎都达到最低版本但 core/prerelease 版本不同时，只报告差异，build metadata 差异不算版本不匹配。更高版本会标为 `above_baseline_unverified`。`doctor` 只检查可执行文件版本，不检查认证、系统/managed policy、桌面 UI 或 daemon 兼容性；版本差异不代表这些路径已验证。`--json doctor` 提供结构化结果；详见英文 [Command reference](Command-Reference)。

一个提供方 = **一个 Responses-compatible 端点 URL + 一把 API 密钥 + 多个模型**。可获取网关模型目录并启动 Codex。兼容性取决于网关和具体模型；提供方没有 ChatGPT 额度视图，也不参与自动选号。别名（Alias）是唯一对用户可见的名称；思考等级（reasoning）与 `web_search` 按**模型**保存，不是按整个提供方。

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

# 启动（使用 `$CODEX_HOME` 中本次运行专属的 `cs-*.config.toml` 原生 profile，不替换 auth.json、不改写 config.toml；`--model` 在 `--` 前须是已保存的模型 id）
codex-switch launch openrouter
codex-switch launch openrouter --model deepseek/deepseek-r1-0528
codex-switch provider probe openrouter
codex-switch provider probe openrouter --model deepseek/deepseek-r1-0528
codex-switch launch openrouter -- exec --json "review this"
codex-switch launch openrouter -- -s workspace-write -a never
```

密钥约定：

- **永远不要**把 API 密钥写在命令行参数里。
- `provider add` 用隐藏输入读取密钥；脚本用 `--api-key-stdin` 从标准输入读。
- 密钥存在 `$CODEX_SWITCH_HOME/providers/<别名>/provider.toml`（目录 `0700`，文件 `0600`），`list` / `show` / JSON / TUI 只显示打码形式（`…` + 末四位）。
- `launch` 时密钥只注入 Codex 子进程环境变量（默认 `CODEX_SWITCH_<别名>_KEY`），不出现在进程 argv。

限制：

- Codex 目前只支持 `wire_api = "responses"`；DeepSeek 官方 Chat Completions API 不能直连，须走 OpenRouter 等网关。同一网关上 `/models` 有 slug 也不等于 `/responses` 能用。`provider probe` 只 POST `{"model":"..."}`（不带 `input`），不走补全。探测结果保存 7 天；`launch` 遇到已保存的“不支持”结论时不会直接拒绝，而是先做一次实时探测：仅在再次确认不支持时才拒绝，探测结果为支持则放行并更新记录，结果不确定或请求失败则放行并在 stderr 给出警告、清除该过期结论。
- 提供方 `launch` 不再使用隔离的 Codex home，而是在共享的 `$CODEX_HOME` 中为每次运行生成 `cs-*.config.toml` 并用 `--profile` 选中，只保存本次的模型、提供方和目录；MCP、skills、插件、hooks、提示词和会话都直接用你现有的，不需要复制或退出时合并，也不会改写用户 `config.toml` 里的 ChatGPT 键。可同时开多个提供方。不能再另行传入 `--profile` / `-p`。
- 提供方 `http_headers` / `env_http_headers` 中的非 ASCII 头值现在可以正常使用，不会再让模型拉取、探测和指纹计算失败。
- 提供方保存的 API key 通过 `env_key` 使用，不能同时为当前提供方设置 `auth` / `auth.command`；添加时会在读取密钥或拉取模型前拒绝这类配置。旧版本保存的此类提供方会显示为需要处理：启动等使用会被拒绝，但仍可用 `provider remove`（或 TUI 删除）移除；`provider rename` 会提示先删除该覆盖项或重新添加。
- `use` 与无别名的 `launch` 自动选号**仅面向 ChatGPT**，不会自动选提供方。
- 提供方别名不能与 ChatGPT profile、其他提供方或 Codex 保留 id（`openai` / `ollama` / `lmstudio`）冲突。
- 删除提供方**不可恢复**（不像 ChatGPT profile 会进 `deleted-profiles/`）。

完整说明见英文 [Custom API providers](Providers)。

TUI 启动的额外 argv 支持带引号参数（如 `--cd "D:\My Work"`）或 JSON 字符串数组（如 `["--cd","D:\\My Work"]`）。引号中的反斜杠保持原样，包括 UNC 路径和末尾目录分隔符；参数本身含引号时使用 JSON 数组。无效输入保留在编辑框并显示错误，不会启动。读取账号/提供方目录失败时保留已有列表并持续提示旧数据或不完整状态，成功重载后再消除提示。

## TUI 操作说明

运行 `codex-switch tui`。四页：**Accounts**（ChatGPT 额度与选号）、**Providers**（自定义提供方）、**Settings**（编辑 `config.toml`）与 **Logs**（本次会话诊断）。`Tab` / `Shift+Tab` 循环切换；`h` 帮助；`q` 退出。设置未保存时，退出前会要求确认。TUI 内按 `h` 看到的快捷键表与代码同源，以当前版本为准。鼠标可点 Tab、列表行、Settings 字段、提供方表单、启动选择器和确认框的 `y`/`n`；这些弹层不会点到背后的页面。

设置 `NO_COLOR` 时，**CLI** 仍遵守无颜色；**TUI** 仍使用设计好的深色配色，避免浅色终端把按键提示洗成黑字。

### Accounts 页

| 键 | 作用 |
|---|---|
| `j` / `k` 或方向键 | 移动选中行 |
| `Enter` | 打开账号菜单；若已勾选多账号则打开批量菜单 |
| `/` | 过滤账号 |
| `r` | 刷新当前可见账号 |
| `a` | 添加账号 |
| `u` | 在主 Accounts 页未勾选账号时切换选中账号；状态栏显示 `Switching to …` 表示进行中 |
| `o` | 打开启动选择器（Codex 默认，或选缓存的模型、本次 reasoning 和额外参数），再用选中账号启动 Codex |
| `Space` | 勾选 / 取消勾选（批量操作） |
| `t` | 开关自动刷新 |
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

编辑 `$CODEX_SWITCH_HOME/config.toml`（代理、缓存、并发、TUI 自动刷新、选号、切换后是否重启 app-server daemon 即 `use.restart_app_server`，以及 launch 恢复延迟）。`j` / `k` 移动字段，`Enter` 编辑或开关，`s` 保存；也可以点击字段编辑或开关，滚轮移动字段。每个字段以其 `config.toml` 键名标示（如 `tui.auto_refresh_interval_secs`、`use.restart_app_server`、`launch.restore_delay_secs`），聚焦字段时列表下方显示简短说明。TUI 进程内立即生效。Accounts 页的 `s` 仍是排序，`t` 只控制当前会话的自动刷新；预热用账号菜单 `w` 或一次性 CLI `warmup`。保存会重写整个配置文件，不保留注释。未保存的修改切走 Tab 仍会保留；正在编辑字段时 `Tab` 不会切页，`Esc` 取消当前编辑。详情以英文 [Configuration](Configuration) 为准。

### 提供方表单（新增 / 编辑）

新增与编辑共用一张表单：

- **新增**：打开后直接输入 Alias；`Enter` 提交当前字段并进入下一项（Alias → URL → Key → Models；env key / wire API / extra `-c` 保持默认）。也可以直接点击某一栏（含 HTTPS 开关和模型行）切换过去。
- **编辑**：从 Base URL 的导航态开始（避免 `s` 被当成输入字符）；`Enter` 进入当前格编辑。
- `Tab` 走遍每一栏，包括 Env key、Wire API、Extra `-c`；在 Models 内用 `j` / `k` 移动。模型很多时表头和底栏帮助钉住，只滚动模型视口并跟着光标；超出一屏时标题显示 `n/N`。Extra `-c` 可输入单个原样 `KEY=VALUE`，或 JSON 字符串数组，例如 `["temperature=0","instructions=a, b=c"]`；逗号不会用于拆分。编辑已有提供方时，此栏以 JSON 数组显示，确保保存值完整保留。
- 模型列表最后一行是 **`+ add model`**：`Enter` 或 `+` / `=` / `a` 添加模型并输入 id。导航态按 `f` 从接入站 `GET /models` 拉取对话模型（去掉 embedding / reranker；超过 48 条打开选择器：`/` 过滤，`space` 勾选，`Enter` 应用）。
- `←` / `→` 切换该模型的 reasoning；`w` 开关 `web_search`；`*` 标为默认模型。
- `d` / `-` / `Delete` 删除模型前会弹出确认（`y` 删除，`n` 或 `Esc` 取消且**不关闭整张表单**）。至少保留一个模型，**最后一条不能删**。
- 编辑时 API Key 留空表示**保留原密钥**。
- `s` 保存；`Esc` 取消整张表单。
- 改名在列表按 `n`，表单里没有第二个「显示名」字段。

启动选择器（Providers 上 `Enter` / `o`）：`←` / `→` 只改**本次会话**的 reasoning，不写回提供方配置。选 `(skip)` 只清除本次启动的默认思考等级（在本次运行的原生 profile 和目录里），保留模型声明的能力，不改动已保存的提供方，也不会临时改写共享 `config.toml`。`Tab` 编辑本次额外的 Codex argv（空白拆分）。

Codex 在前台运行；退出后回到 TUI。

## 参与开发版测试

开发版属于滚动 prerelease 通道。安装、验证、回退和问题反馈步骤见 [Testing development releases](Development-Releases)，其中附有中文摘要。

当前滚动开发版示例：`codex-switch self-update --dev`，版本号形如 `20260828.1.0-dev`。

## 常用入口（英文正文）

- [开始使用](Getting-Started) — 安装、登录和首次启动
- [功能指南](Feature-Guide) — 主要工作流与安全边界
- [自定义 API 提供方](Providers) — CLI、存储、`provider.toml`、OpenRouter / DeepSeek 经网关
- [命令参考](Command-Reference) — 全部命令、全局选项与完整 TUI 表
- [配置](Configuration) — 路径、代理、缓存、选号与 launch 设置
- [更新](Updating) — 更新方式、通道切换和旧版本迁移
- [故障排查](Troubleshooting) — 常见错误与恢复方式
- [常见问题](FAQ) — 简短问答

命令行为以已安装版本的 `codex-switch <命令> --help` 为最终依据。

## 反馈问题

提交 Issue 时请附操作系统、终端、`codex-switch --version`、完整命令、预期结果、实际结果与最小复现步骤。分享 debug 输出前必须删除 Token、提供方密钥、邮箱、account ID、工作区名称、可识别身份的路径和代理凭据。

[提交 GitHub Issue](https://github.com/xjoker/codex-switch/issues)

## Next steps

- 第一次使用：继续阅读[开始使用](Getting-Started)。
- 日常操作与可选系统计划任务：查看[功能指南](Feature-Guide)。
- 提供方与模型报错：先看英文 [Providers](Providers) 与 [故障排查](Troubleshooting)。

### 提供方的原生 Codex 环境

提供方启动按 Codex 0.159.2 的模型契约验证（见仓库 `docs/CODEX-ALIGNMENT.md`）。每次启动在 `$CODEX_HOME` 创建 `cs-*.config.toml`，只保存本次提供方的差异配置；密钥仅通过子进程环境变量传入。公共资源与周边服务认证继续由 Codex 原生机制管理。Codex 内保存的设置属于当前 profile，恢复该会话时继续使用；公共配置的后续更新仍会被继承，除非该 profile 已覆盖相同设置。不要另行传入 `--profile` / `-p`。

运行元数据与模型目录保存在 `$CODEX_SWITCH_HOME/provider-runs/`，会话保存在默认 Codex home。提供方重命名不会改变历史归属，删除后同名重建不会接管旧会话。已有旧版隔离目录的历史仍按旧路径恢复，尚未迁移为共享资源环境。

JSON 模式使用运行目录中的 `stdout.jsonl` 和 `stderr.txt` 承接子进程输出，正常完成后读取并清理；启动工具异常退出时保留输出和 profile。主动中断仍会终止子进程；关闭整个终端或操作系统杀掉进程树不属于仅启动工具崩溃。
