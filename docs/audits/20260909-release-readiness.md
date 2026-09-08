# 发布候选资格记录（2026-09-09）

候选基础版本：`20260909.1.0`。开发分支：`dev`；基线：`be6a0add0652a1e8ea34f93852d74908c9e81497`。本记录描述本地证据，不代表 GitHub 已发布或所有目标平台已验收。

## 裁决

`UNKNOWN`：本地已验证的检查通过，仍缺少 macOS/完整发布矩阵的 CI 结果；provider 跨运行 resume 策略仍待用户决定。当前没有发布正式版本、移动 dev tag 或合并 master。

## 本地检查

| 检查 | 真实结果 |
|---|---|
| 凭据、启动、TUI、网络与缓存审查 | R1/R2 初审各两项问题，修复后的定向复审通过 |
| 安装、更新与供应链安全审查 | R3 九项问题完成修复并通过定向复审 |
| Windows 测试 | 560 库 + 140 集成，共 700 项有通过证据；初次失败及定向修复详见消融记录 |
| Linux 测试 | 全量 572 库 + 146 集成，共 718 项通过；最后 HTTP fixture 修改后的启动 9 项再验证通过 |
| Clippy | Windows/Linux `--all-targets -- -D warnings` 通过；最后 Windows fixture import 修改后对应 target 再验证通过 |
| 格式 | `cargo fmt --check` 通过 |
| 依赖审计 | cargo-audit 0.22.2 检查 384 依赖、1242 公告，无报告，退出 0 |
| Windows 优化构建 | `cargo build --locked --release` 通过 |
| 产物冒烟 | `--version` 返回候选版本；假账号 `use demo` 后 live/current 一致；`daemon --help` 退出 2 |
| TUI 运行态 | 隔离假账号主界面 `u` 显示 Switching 后激活，`q` 正常退出 |
| 旧版升级 | 固定哈希的官方 Windows v0.0.19 二进制从本地 metadata/候选 archive 实际升级到 20260909.1.0 |
| 安装器 | Windows 4 项、Linux 2 项，均通过；未操作真实系统任务或用户 PATH |
| 文档/Wiki | 已更新仓库源文件；链接和发布契约测试通过，线上 Wiki 尚未同步 |

Windows 测试以一次全量入口和失败后受影响集合组成证据，未声称单次全量零失败。完整红灯与转绿说明见[测试消融](20260908-test-ablation.md)。本机日志位于忽略目录 `target/verification/`，不随 Git 推送。

## 待闭合

- provider 每次启动创建独立 CODEX_HOME，跨次 `resume --last` 没有稳定恢复入口。需要确认采用最近运行目录恢复，或明确接受本候选的已文档化限制。
- macOS 尚未在本机运行；Windows ARM64 的本地编译因缺少 clang 未完成。最终三主机 CI 与六架构构建仍需远端验证。
- 未使用真实账号运行 OAuth、completion 或真实 Codex resume；没有测量预热的真实业务收益。
- Windows 伪终端的 Ctrl+C 字符未触发退出，不能作为 OS 信号退出通过的证据；Linux SIGTERM 子进程回收和 TUI 生命周期测试已通过。

## 远端操作边界

本地代码、文档与版本就绪并明确处理 provider 决策后，才请求集中推送 dev；分支 CI 三平台通过后，再按明确授权触发 dev tag。Wiki 由 dev 的已审查源文件同步。正式 release 和 master 合并始终不在授权范围。

分支归档已验证，参见[分支收敛记录](20260908-branch-retention.md)。尚未删除远端分支；执行前须重查 SHA，并取得裁剪授权。
