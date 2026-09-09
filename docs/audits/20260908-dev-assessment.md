# dev 分支整改评估（2026-09-09）

本报告承接最初的 dev 基线审计，记录当前整改结果和仍未闭合的证据边界。初始基线为 `be6a0add0652a1e8ea34f93852d74908c9e81497`、版本 `20260902.1.0`；当前评估按 dev 整改代码和 `docs/audits/dev-goal-status.md` 整理。

## 当前判断

账号一致性、启动恢复、Use 响应、主列表快捷键、别名边界和 warmup 主池选择已完成对应修复；R1、R2 定向复审通过。daemon 按用户决策删除，预热和切换保持为分开的功能，保留一次性预热与手动切换。R3 的安装、供应链和文档九项问题已全部通过定向复审。

当前不具备发布就绪结论。provider 已采用稳定 identity、持久隔离 run home 和 provider-scoped `resume`；真实 OAuth、真实 completion、真实 Codex resume 和真实业务预热收益均未测。macOS 尚未运行，Windows ARM64 编译受本机缺少 clang 阻塞。

## 原 13 项问题的当前处置

| 编号 | 初始问题 | 当前处置与定位 |
|---|---|---|
| 1 | Use 同步阻塞 TUI | 已修复，R2 复审通过。`src/tui/app.rs::start_switch` 使用后台任务执行切换并回传完成状态，`last_used` 写入失败可见。 |
| 2 | 主界面缺少 `u` 快捷键 | 已修复，R2 复审通过。主列表事件分发和状态提示覆盖当前选中账号。 |
| 3 | 同账号 launch 恢复旧凭据 | 已修复，R1 复审通过。`src/launch.rs::restore_launch_auth` 区分同一凭据轮换与同身份的独立凭据：前者保留新 live，后者恢复原独立凭据。 |
| 4 | 登录已有账号后 current/live 不一致 | 已修复，R1 复审通过。`src/profile.rs::save_auth_value` 更新已有 profile、live auth 和 current 的同一事务路径。 |
| 5 | provider/profile 别名冲突不对称 | 已修复，R1 复审通过。创建、保存和重命名入口统一经过 `reject_provider_alias` 与别名占用检查。 |
| 6 | provider 新 home 缺少稳定 resume 入口 | 已按稳定 identity 与 provider-scoped session locator 实现；待新提交全量测试和 CI 验证。 |
| 7 | Windows `.cmd` 预检与启动不一致 | 已修复，定向启动透传 8/8 通过。`src/launch.rs::ensure_codex_available` 解析出的具体候选路径继续用于启动。 |
| 8 | daemon 探测失败仍允许自动换号 | 按用户决策删除常驻 daemon 与自动换号路径，原风险入口已移除。 |
| 9 | daemon 慢请求阻塞停止和热加载 | 随常驻 daemon 删除处置；不再作为当前产品路径验收。 |
| 10 | daemon 批次失败默认不可见 | 随常驻 daemon 删除处置；一次性预热失败仍按 CLI/TUI 结果反馈。 |
| 11 | TUI 与 daemon 重复调度 warmup | 已按决策拆分预热与切换，删除 daemon 调度，保留一次性预热和手动切换；R3 相关修复已复审通过。 |
| 12 | 附加额度池模型被当作主池成功 | 已修复并补回归契约。`src/warmup.rs::select_warmup_models` 要求主池模型先成功，再组装附加池模型；CLI warmup 入口 `src/commands/misc.rs::warmup_cmd` 先校验别名。 |
| 13 | 详情模型失败后每帧重试 | 已修复，R2 复审通过。`src/tui/app.rs::ensure_models_loaded` 对 Error 状态短路，显式刷新才重新请求。 |

## 可复现事实与覆盖范围

初始隔离实验使用假凭据、真实临时文件锁和本地 fixture，不启动 daemon、不访问真实账号或网络：无竞争切换约 8–15 ms，持有 launch/auth 锁约 10 秒；缓存锁也能造成约 10 秒等待。Windows 命令解析实验确认只有 `.cmd` 包装器时不带扩展名启动会失败，使用解析出的明确路径可成功。

账号标记复现和缺陷注入记录仍见[隔离复现说明](20260908-reproduction.md)；测试裁剪、三项缺陷注入及恢复结果见[测试消融与回归验证](20260908-test-ablation.md)。这些证据证明文件、锁和本地入口契约，不证明真实 OAuth、真实 completion 或真实业务收益。

原始覆盖包括 auth/profile 持久化、launch/provider 隔离与恢复、TUI 事件和按键、warmup/config、缓存、安装契约及相关 daemon 代码路径。当前文档只复核整改涉及的最终函数和测试证据，不重新开放全库审计，也不把测试数量当作覆盖率。

## 测试状态与剩余边界

- 原 Windows 可运行测试数 740 作为基线记录。
- 当前库测试数 560：初次运行 559 项通过、1 个 fixture 失败；失败 fixture 只有附加池模型，与“附加池不得替代主池”的新契约冲突。补入主池模型后，受影响的 2 项通过。
- Windows 合计 700 项测试有通过证据；Linux 全量 718 项通过，HTTP fixture 更新后的 Linux 启动 9 项复测通过。Windows/Linux Clippy 和格式检查通过。
- Windows 优化构建通过，release 产物的假账号 use 和已删除 daemon 命令边界冒烟通过；固定哈希的官方 v0.0.19 在隔离目录实际升级到本候选版本。
- R1、R2、R3 定向复审通过，原审查 findings 已关闭。
- provider resume 的真实 Codex 交互、真实 OAuth、真实 completion、真实预热收益尚未验证；macOS 尚未运行，ARM64 仍缺 clang。

本评估保留已验证的锁竞争、凭据恢复、别名/输入边界、warmup 主池和 Windows 启动证据；未将未测的真实业务效果、跨平台结果或 provider 会话恢复写成已完成。
