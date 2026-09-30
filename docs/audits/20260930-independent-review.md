# 独立代码审查（2026-09-30）

## 范围与证据边界

本报告记录只读审查的代码基线 `97a220b`、候选版本 `20260930.4.0`。三位 Luna 审查者分别检查了认证/策略、provider 与启动、usage/评分及 TUI 行为；复现和代码阅读结果按证据强度区分。审查未读取真实用户凭据，也未向真实 Codex 或 OpenAI 端点发送请求。

本报告是行为与删减建议，不是发布批准。最低 Codex 版本检测和桌面兼容是在 `97a220b` 基线之后新增的工作，见下方后续验证记录。版本与模型契约按 Codex 0.159.2 核对，本轮未发现需要以“落后于该版本”描述的问题。只读审查阶段，主 Agent 运行 `cargo rustc --lib -- --force-warn dead_code` 并通过，其结果用于下文生产无用项盘点。

## 基线后的版本门禁验证（开发前阶段记录）

在上述审查基线之后新增的最低版本门禁和 `doctor`，最低版本与当前对齐版本均为 `0.159.2`。`launch` 在账号选择、consume、provider run 创建和 auth staging 前，对同一已解析 PATH 可执行文件执行 4 秒版本探测；`doctor` 可另行检查显式桌面 engine。它只判断执行文件版本，不验证认证、系统/managed policy、桌面 UI 或 daemon 兼容性。

在版本门禁完成、下列 P2 修复开始前，主 Agent 完成本地质量检查：`cargo test --all --quiet` 通过 872 项（696 unit、176 integration；7 个 integration binaries 有测试，Windows `test_tui_shutdown` 为 0 个适用测试，doc tests 为 0）；`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo audit`（384 crates、1,277 RustSec advisories 无告警）、`bash -n scripts/install.sh`、PowerShell installer 解析和 `git diff --check` 均通过。

官方 Codex 0.159.2 对 PATH CLI 和同一桌面 engine path 的检查返回 `aligned` / `versions_match: true`。隔离 JSON cases 覆盖：0.160.0 高于 baseline 时允许并标记 `above_baseline_unverified`；0.159.1 返回非零；0.159.2 build metadata 变化仍为 aligned/match；并确认 JSON 单 envelope、stdout-only、stderr 为空。测试使用临时目录或 fake executable，不涉及真实账号或生产请求。

这段记录描述当时未提交、未推送的 Windows 本地工作树。修复后的当前验证见下一节；跨平台 CI 与 release artifact 验证仍未完成。候选为 `20260930.4.0`，本地质量通过不表示已发布。

## 开发后的修复与验证

三位 Luna 子 agent 分别修改认证、供应商/端点和 TUI，并交叉只读复核；主 Agent 负责整合、独立检查与统一质量门禁。以下六项基线发现均已修复：

| 基线发现 | 最终行为与回归覆盖 |
| --- | --- |
| WIF 文件切换误报成功 | CLI 与直接 profile/TUI switch 都检查两个 federation 变量的存在性，包括空值；失败时 live auth/current 不变。登录、导入和 ChatGPT 请求在对应副作用前拒绝，provider-key launch 保持独立。 |
| managed auth 来源缺失 | 只读 resolver 合并用户/legacy defaults、系统 requirements 和强制 macOS MDM；requirements 覆盖普通 defaults，工作区限制取有效交集。Windows 从 OS Known Folder 获取系统 ProgramData；Unix legacy 文件位于 `/etc/codex/managed_config.toml`。损坏、过大或不支持的 endpoint 策略明确报错。 |
| provider auth 与 env_key 冲突 | TOML 键结构检查覆盖 active provider 的 dotted/quoted/table `auth`，也处理 bare/empty RHS。add 在读密钥/取模型前拒绝；load/save/launch 也校验。 |
| TUI extra argv 引号错误 | 支持 literal-backslash quoted argv 和 JSON 字符串数组，保留 UNC/尾反斜杠；错误输入保留并渲染错误，不 launch。 |
| TUI 列表读取失败清空 | 分域暂存后提交；失败保留旧行、选择、usage 和标记，持续显示 stale/incomplete；需要成功 reload 的刷新被阻止。 |
| reset consume 未继承 usage override | GET/POST 使用统一派生优先级，保留 query/尾 slash；需要派生的无效 URL 在请求前失败，consume 归类为 definitely-not-consumed。 |

交叉审核还修复了两处关联问题：刷新期间策略变化时，先 CAS 保存已轮换的新 profile token，再判断是否更新 live auth；策略拒绝不丢新 token，真正 live I/O 错误仍报告。usage 刷新输掉 CAS 时不使用或缓存失败方响应；只有磁盘三项 token 完全等于同一响应时才视为重复保存成功，正常多轮重试继续成立。严格 import/reauth 身份边界保持独立。

最终 `cargo test --all --quiet` 通过 **906 项**（724 unit、182 integration，8 个含测试的 integration binaries，0 ignored）。fmt、Clippy `-D warnings`、依赖 audit、Git Bash shell installer syntax、PowerShell installer parse 和 diff check 均通过。隔离 CLI smoke 用官方 Codex **0.159.2** 检查 PATH CLI 与同一桌面 engine，返回 aligned/match；WIF launch 返回单 JSON 错误，未创建 live auth 或 provider-run state。测试只使用假凭据、mock/loopback 服务和临时目录。

另一个隔离 smoke 在 WIF 存在且用户 store 为 keyring 时，使用假 key 添加 provider 并通过官方 Codex 的 `--help` 完成 provider launch；确认返回单 JSON、没有创建或交换 `auth.json`。这验证本工具的独立 provider 分支和进程启动，不代表真实供应商 completion 或企业环境认证已实测。

首次统编曾被并行未完成的 test 接口阻挡，不将这种编译失败记作语义型 fail-before。基线的 WIF、provider conflict 和 URL 路由已有上文独立复现证据。整合阶段实际出现的 identity、live-I/O 和重复 persist 回归已修复，最终全量测试覆盖这些契约；交叉独立复核未发现遗留 CRITICAL/HIGH。

上述验证记录来自 Windows 本地。首次推送 `334ff14` 后，三平台 CI 的测试步骤均失败；后续修复将 Windows 路径分隔符断言限制到 Windows，并拆分 Unix 路径测试，本地全量通过 907 项（725 unit、182 integration）。CI 增加失败摘要注释，以便读取后续失败诊断。发布必须以候选提交的三平台 CI 和 Release workflow 结果为准；实际企业策略部署、真实账号 quota、桌面 UI/daemon 尚未实测。

## 已复现 / 代码确定的问题（基线，现已修复）

### [P2] `use` 可能在 Codex 无法读取认证文件时报告成功

**证据：隔离假凭据进程复现。** 当前复现条件是父环境存在 `OPENAI_FEDERATION_RULE_ID`、但没有 `OPENAI_IDENTITY_TOKEN_FILE`。codex-switch 的 `use` 会成功写入并切换文件，随后官方 `codex login status` 因缺少 `OPENAI_IDENTITY_TOKEN_FILE` 失败。这里 WIF 指 workload identity federation（工作负载身份联合），不是 Windows Identity Foundation。触发时 Codex 的 federation auth 优先级会使普通文件凭据路径不可用；复现使用隔离目录和假 auth，不涉及真实账号。

建议 preflight 清晰拒绝或提示检测到不完整的 WIF 环境，并说明所需变量；不要静默删除或改写企业认证环境变量，也不要把仅检查文件写入成功等同于 Codex 登录可用。参考 OpenAI [Authentication](https://learn.chatgpt.com/docs/auth) 与 [Troubleshooting](https://learn.chatgpt.com/docs/reference/troubleshooting)。

### [P2] managed auth 预检覆盖面窄于 Codex 的配置来源

**证据：实现确定，Codex 最终拒绝行为的政策边界已核对。** `src/auth.rs:142-263` 的校验只读取 `$CODEX_HOME/config.toml`，覆盖 `forced_login_method` 与 `forced_chatgpt_workspace_id`。系统级 requirements、MDM 管理值以及官方用户配置字段 `chatgpt_base_url` 等来源没有进入此预检。特定受管配置下，codex-switch 因未识别约束而接受写入，之后由 Codex 按自身政策拒绝认证。它是诊断/兼容缺口，不代表 codex-switch 绕过或改变 Codex 政策。

建议集中成 `AuthContext` / `doctor` 类诊断，报告发现的配置来源和可能冲突；来源无法读取时明确标为未知，不应静默宣称验证了完整政策。参考 OpenAI [managed configuration](https://learn.chatgpt.com/docs/enterprise/managed-configuration) 与 [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)。

### [P2] provider `auth.command` 与生成的 `env_key` 不兼容

**证据：隔离动态复现，假 key、无联网。** 使用临时 `CODEX_HOME` / `CODEX_SWITCH_HOME`，经 `provider add --api-key-stdin --set 'model_providers.audit.auth.command="nonexistent-audit-token-command"'` 保存该配置，命令成功并返回 `provider-added`；再由官方 Codex 0.159.2 加载同一 provider 的 `auth.command` + `env_key` 时，`codex login status` 以 `cannot be combined` 拒绝。`src/provider.rs:560-580` 为 provider profile 固定生成 `env_key`，`:610-615` 同时把保存的 API key 放入该环境变量。该互斥来自 Codex 配置契约（`auth.command` 不能与 `env_key`、`experimental_bearer_token`、`requires_openai_auth` 并用）。因此 provider add 接受了必然无法由当前生成配置启动的组合。

建议在验证 Extra `-c` 或组装有效 provider 配置时检测互斥字段并给出具体错误。不要暗中删除用户的 `auth.command` 或固定 `env_key`；若要支持自定义 auth command，需要先定义它与保存密钥/环境变量的互斥语义。

### [P2] TUI 启动额外参数不支持带引号的空格

**证据：实现确定。** `src/tui/provider_launch.rs:210-217` 使用 `split_whitespace()` 拆分额外 argv。输入 `--cd "C:\path with spaces"` 会被拆成多个 argv，导致 Codex 收到错误参数。建议改成明确的 argv 编辑器/分词器，或把 UI 限定为逐项编辑参数；不能将 shell quoting 描述为受支持语法。

### [P2] TUI profile 目录读取失败会清空当前列表

**证据：实现确定。** `src/tui/app.rs:1317-1322` 在 `list_profiles()` 目录读取失败时记录 warning 后以空列表替代，后续将其赋给 `self.accounts`。临时目录 I/O 错误会让用户误以为没有已保存账号，并可能使后续操作基于空列表。单个 auth 文件损坏有另一条逐 profile 读取路径，不应与此混为一谈。建议保留最后一份可用列表，并在 UI 明示目录加载失败；只有成功读取后再替换状态。

### [P2] 仅用 `CS_USAGE_URL` 覆盖时，reset-card POST 仍使用正式端点

**证据：路由函数安全复现；只打印 URL、没有网络请求。** `src/usage/api.rs:207-209` 将 `CS_USAGE_URL` 用作 Usage GET 地址。`src/usage/reset_credits.rs:100-123` 的 GET reset-credit 地址会从它推导，但 consume URL 仅在设置 `CS_RESET_CREDITS_URL` 时才从本地地址推导；仅设置 `CS_USAGE_URL` 时会落回生产 consume URL。用临时 `rustc` harness 执行原文件中的常量和 URL 函数，仅设置 `CS_USAGE_URL=http://127.0.0.1:9/backend-api/wham/usage` 时，GET 为本机 `/rate-limit-reset-credits`，POST 却为 `https://chatgpt.com/.../consume`。因此这是**只在开发者/测试者仅覆盖该环境变量且触发消费时**的路由安全风险，并非所有用户都会触发；真实 backend 未调用。应统一环境覆盖契约，让所有相关请求都路由到测试端点。

## 条件风险与兼容性边界

- **workspace/email 身份匹配不应放宽。** `account_id` 可表示 workspace 而不是用户。仅凭相同 account id 可能把同一 Team 的不同成员合并；缺少可靠用户标识时宁可要求显式选择。当前审查没有建议恢复“account_id 优先、email 次之”的旧式猜测。
- **导入 `validated_account_id` 可作为类型不变量收敛。** `src/commands/import.rs:372-380` 的后续步骤要求该 ID 存在；成功但缺 ID 的中间状态不可达。建议只做类型/控制流表达上的收敛，不将其视为当前功能缺陷。
- **warmup 的真实收益仍未验证。** Mock HTTP 能证明请求/重试状态机，不能证明真实账号会打开预期 quota window 或改善后续请求体验。保持现有一次性 `warmup`，在有安全、明确的隔离测量方案前，不建议据此删除或扩展该功能。

## 可裁剪与维护建议

- **收敛生产无用接口。** 主 Agent 的 `cargo rustc --lib -- --force-warn dead_code` 通过，结果显示以下项目没有生产调用：`provider.rs` 的 `fetch_gateway_models_at` / blocking wrapper、`ResponsesProbe.refusal_message`、`probe_responses_support`、`ProviderHomeInput`、`ProviderCodexHome::begin` / `write_model`、`ProviderSession::run_id`、`SessionIndex::is_empty`、`warmup_cache_key`，以及 `cache.rs` 的 `apply_usage_updates`、`put_many`、`put_many_async`。`src/lib.rs` 对多个模块有 blanket `allow(dead_code)`，因此这份清单需要结合调用图审查。先区分仅供单元测试的入口（标为 `#[cfg(test)]` 或移入测试辅助模块）与已淘汰兼容路径，再删除；不可仅因 begin/write_model 无生产引用就删掉整个 `ProviderCodexHome`，因为 `src/launch.rs` 仍通过 `open_existing` 恢复旧 provider 会话。
- **削减实现镜像式测试。** 优先保留行为回归、跨进程、并发、mock HTTP 和真实配置契约测试。只验证源码字符串、固定文案/章节标题或标准库行为的用例维护成本高，代码重构时易产生无价值失败；发布产物、校验和、provenance、JSON 协议等外部契约测试有独立价值，应保留。
- **缩短中文指南。** `docs/wiki/Chinese-Guide.md` 已声明英文 Wiki 为规范，但仍重复大量 CLI、超时、daemon 状态、TUI 表单和 provider resume 细节。建议保留中文快速上手、常用命令和关键安全边界，将精确行为与快捷键表链接到英文规范页，降低双份规格的漂移风险。
- **评分加强需按模型额度池建模。** 账号级汇总百分比可能掩盖模型专属 pool 的耗尽与重置差异。可考虑在评分中使用当前请求模型对应的 pool，并提供解释字段；这是产品强化建议，不是对当前评分结果已证明错误的结论。应先明确 CLI/TUI 没有“当前模型”时的 fallback 规则，再建立可审计的 fixture。
- **以 mock 保持上游契约。** provider config、Responses probe、warmup SSE、app-server 与 auth 文件契约均适合由版本化 fixture 覆盖，并在 Codex 版本升级时显式更新；避免用源码字符串自证行为。Codex 的 `app-server` 提供账户与用量接口，属于减少私有 HTTP API 依赖的候选，但不是本轮重写要求；参考 OpenAI [app-server](https://learn.chatgpt.com/docs/app-server)。
- **ChatGPT launch 的认证就绪确认可作架构实验。** 目前 ChatGPT launch 在 staging auth 后依赖固定等待窗口（基线 `97a220b` 的 `wait_for_codex_to_read_auth` 路径）。可评估 app-server 外部 ChatGPT token 模式（`chatgptAuthTokens`）或隔离 Codex 子进程读取 auth 后回报就绪，再恢复原文件；这不是 workload identity federation (WIF)，也不是建议为 provider launch 新增默认联网鉴权请求。该模式尚未证明能直接替代 codex-switch 当前多 profile 的额度/用量读取。需先验证 token 生命周期、取消/超时恢复和共享 daemon 边界，再决定是否实现；参见 OpenAI [app-server](https://learn.chatgpt.com/docs/app-server) 与 [Authentication](https://learn.chatgpt.com/docs/auth)。

## 建议顺序（审查当时）

前三项正确性修复已完成并验证。以下第四、第五项仍为后续维护/测量建议，本轮没有批量删功能或改变评分/预热策略。`doctor` 仍专注版本检测；认证策略由操作前预检与具体错误来源说明。

1. 处理 `CS_USAGE_URL` 与 consume URL 一致性和 `auth.command` 配置互斥，避免错误路由/启动拒绝。
2. 为 WIF 继承固化隔离复现；补 TUI argv 引号边界和目录读取失败时保留/显式标记旧 profile 列表的回归覆盖。
3. 将 managed auth 预检边界纳入 doctor/诊断，并与 Codex 的系统级策略来源保持清晰区分。
4. 将死代码与镜像式测试按生产使用情况分类后小步删除；保留旧 ProviderCodexHome 的 resume 恢复路径。
5. 把模型额度池评分、ChatGPT launch 本地认证就绪实验及 warmup 收益留在有测量方案的后续改进，不作为当前正确性结论。
