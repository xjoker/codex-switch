# dev 发布整改状态（单一进度源）

## 范围与硬边界
- Goal 启动：2026-09-08 23:07:51 +08:00；截止：2026-09-09 05:07:51 +08:00（6小时）。
- token 不设上限；Agent 总调用 ≤50；Review ≤10；最多4个活跃Agent含主Agent。
- 普通开发/整理 Luna max；复杂根因及独立审查 Sol high。
- 仅 dev 本地开发。禁止合并 master、正式 release；集中 dev 推送/发布需最终确认。master 不改名。
- 删除 daemon 与 TUI 自动预热，保留一次性 CLI/手动操作，系统计划任务由用户配置。
- 修复审计核心问题，测试消融、Windows补全、全项目review、文档/wiki更新、分支收敛。

## 预算计数
- Agent调用：14/50（本goal从0计，启动前审计不计）。
- Review：0/10。
- 本地提交：无。

## 阶段与验收
1. 核心修复和功能裁剪：先红灯契约再实现；凭据一致性、Use响应、Windows命令启动、daemon移除可编译；定向测试通过；本地提交。
2. 验证与全项目审查：Windows核心路径和测试消融，现有行为保护不退化；正确性及安全review阻断关闭；本地提交。
3. 文档与dev发布候选：全部文档/wiki/CLI帮助一致、版本与构建/测试/安装检查通过、旧任务迁移说明；分支独有变更清单；本地提交；请求集中dev触发授权。

## Kanban
| 卡片 | Owner | 状态 | 边界 |
|---|---|---|---|
| A1 账号/启动红灯测试 | Luna | 3项红灯已验证 | profile.rs、launch.rs、Windows启动集成测试 |
| A2 TUI红灯测试 | Luna | 4项红灯已验证，A6实施中 | tui/app.rs、keymap.rs及事件测试 |
| A3 daemon删除边界盘点 | Luna | 已完成，A5实施中 | 只读，配置/命令/测试/依赖/文档引用 |
| 核心实现 | 待定 | 等待红灯 | 不交叉写同一文件 |
| Windows及测试消融 | 待定 | 待执行 | 真凭据和机器真实配置禁区 |
| 全项目审查 | 主Agent/Sol | 待执行 | 全项目冻结契约 |
| 文档/wiki/发布收尾 | 待定 | 待执行 | 不触发远端发布 |

## 已有证据
- 审计基线 be6a0add0652a1e8ea34f93852d74908c9e81497；原有Windows库测试612、集成128通过。
- Use同步锁等待、已有登录current/live不一致、同账号launch旧凭据恢复已隔离复现。
- 已通过 Git OpenSSL 后端核实远端 dev/master 与本地引用一致。
- 详见 docs/audits/20260908-dev-assessment.md；原审计副本 target/audit-dev。

## 未决/验证边界
- provider会话恢复需真实机制验证后决定最小修改，不能直接删除历史目录。
- 真实账号预热业务收益未测，不能以mock成功替代。
- Linux/macOS平台验证若环境不可用须明确报告。



- A4：账号/启动三项修复实施中；A5：daemon删除实施中。两者ownership互不重叠。
- 红灯实测：三测试各exit101，证据 target/verification/*-red.txt。


- TUI四项红灯全部exit101，基线副本编译运行。源码分发断言会在实现后替换真实事件入口测试。
- cargo-audit 0.22.2：RustSec 1242条公告，锁文件387项依赖，退出0，无漏洞报告（后续锁文件删依赖需最终复核）。

- Linux验证环境：现有Ubuntu-24.04 WSL可访问，~/.cargo/bin/cargo 1.90.0、cc和curl存在；需在代码冻结后验证依赖MSRV和运行测试，尚未声称通过。
- Windows Rust目标：x86_64-pc-windows-msvc与aarch64-pc-windows-msvc均已安装。

- A4已完成；隔离副本profile 59、launch 31测试全绿；三条红灯均修复。
- A7别名冲突测试准备中，A5/A6继续实施。


- A5完成daemon核心删除；Cargo workspace更新移除chrono-tz 0.10.4、phf/phf_shared 0.12.1，无依赖版本升级。
- A7三项名称冲突红灯确认；A8实施中。A9预热正确性/非法alias先行测试准备中。


- A8别名冲突实现完成，等待绿灯；A9预热两项测试完成，等待红灯；A10测试消融盘点进行中。


- A8验证：profile 62通过；A9两项红灯确认（主池误选、穿越alias）；A11最小修复实施中。


- A11预热修复完成待绿灯；A12 Windows启动契约补全进行中；A13文档/wiki同步进行中。A10测试清单已完成，区分删除功能70项与保留行为的消融。


- 统一Windows库测试编译成功；TUI定向114通过，A14补切换期间冲突动作门禁。预热两项红灯已转绿。
- 三项隔离缺陷注入均被测试检出（各exit101），恢复后各exit0：CAS、同账号launch恢复、unattended cache。

