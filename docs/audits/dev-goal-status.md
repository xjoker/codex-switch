# dev 发布整改状态（单一进度源）

## 范围与硬边界
- Goal 启动：2026-09-08 23:07:51 +08:00；截止：2026-09-09 05:07:51 +08:00（6小时）。
- token 不设上限；Agent 总调用 ≤50；Review ≤10；最多4个活跃Agent含主Agent。
- 普通开发/整理 Luna max；复杂根因及独立审查 Sol high。
- 仅 dev 本地开发。禁止合并 master、正式 release；集中 dev 推送/发布需最终确认。master 不改名。
- 删除 daemon 与 TUI 自动预热，保留一次性 CLI/手动操作，系统计划任务由用户配置。
- 修复审计核心问题，测试消融、Windows补全、全项目review、文档/wiki更新、分支收敛。

## 预算计数
- Agent调用：33/50（本goal从0计，启动前审计不计）。
- Review：3/10。
- 本地提交：5dbf118（账号/启动/预热修复检查点，待最终独立审查）。

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



- A12 Windows启动透传补全完成，实际集成测试运行中；A16修复卸载脚本调用已移除daemon命令的连带问题（先红灯）。


- A15 TUI门禁实现完成，A17 Windows启动HTTP测试fixture修复进行中。新锁文件cargo-audit：384依赖、1242公告，exit0。
- 22个远端引用完整历史bundle已验证；详见分支收敛记录，未删除远端。


- A16先实施后提交测试，缺少严格先红顺序证据；A18修正卸载测试隔离（Unix legacy路径、Windows用户PATH），未运行有风险fixture。
- A15门禁定向测试由红转绿。Linux首次编译进行中。


- R1凭据/启动/provider全模块独立Sol high审查进行中；A20删除3个纯文本/实现镜像测试并清理失效注释。Linux库测试编译已成功。
- A13文档/wiki更新完成，仍待最终交叉核对及changelog/发布资格记录。


- Windows真实伪终端冒烟：临时假账号demo，本地拒绝端口隔离网络；主列表u显示Switching后active，q退出exit0，current与live account均demo。未使用真实账号。
- 修复fixture后Windows启动集成8/8通过。


- 本地检查点4683b5d：daemon裁剪及TUI响应修复；与5dbf118分开可回滚，均待最终审查。


- R2 TUI/网络/缓存/配置完整模块Sol high审查启动。A20完成3项冗余文本测试消融。
- Windows ARM64检查因本机缺clang未能完成（依赖C编译阶段），不是已验证通过。


- 用户中断后明确继续；A22续R2、A23续卸载安全fixture、A24准备R1两项红灯测试。Review仍2/10。
- R1发现同身份不同凭据恢复及import/login冲突预检两项P2，待修复。Goal工具显示paused；本轮按用户继续授权执行，原墙钟截止不延长。


- R3更新/安装/供应链独立Sol high审查启动。卸载旧脚本+当前新版binary隔离实测红灯：无法识别daemon，exit101；未操作真实任务或User PATH。


- R2发现自动刷新可重新阻塞Use（P1）、模型刷新旧回包覆盖新状态（P2）；A26先行测试准备。Windows卸载两项已绿。


- R1三条边界回归已实测红灯；A27最小修复实施中。R2测试准备中。WindowsCtrl+C字符未触发退出，随后q正常退出；未将此冒烟计为OS信号退出验证。


- A27凭据审查修复完成待绿灯；A28 TUI审查修复实施中。R3发现发布前验收顺序、Homebrew hash注入两项P1及安装/文档边界；A29修CI供应链、A30修安装/行尾契约。Review累计3/10。


- 用户再次中断后继续：A31/A32/A33分别续原TUI、CI、安装工作；Review保持3/10，截止05:07:51不延长。
- A27定向验证：启动相关32通过，import/login网络前置两项由红转绿。

