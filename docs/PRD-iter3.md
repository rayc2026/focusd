# focusd 增量 PRD（迭代三 · 常驻韧性）

> **性质说明**：本 PRD 是**增量文档**，只描述本次迭代的变更部分。
> 基线是 v0.2.0（迭代二产出）：`org.focusd.Focus1` D-Bus 服务 + 三后端（wlroots 推送 / KWin 推送 / GNOME 轮询）+ systemd user unit + CI 三 job 全绿。
> 基线功能与契约不在本文重复，见 `README.md`、`docs/PRD-increment.md`、`docs/ARCHITECTURE-increment.md`、`docs/dbus.md`。
>
> 日期：2026-09-14 ｜ 作者：产品经理 许清楚

---

## 1. 项目信息

| 项 | 值 |
|---|---|
| Project Name | `focusd` |
| Language | 中文 |
| Programming Language | Rust 2021（clap + zbus 5 blocking + wayland-client，**无 tokio**） |
| 迭代主题 | **常驻韧性（Residency Resilience）** |
| 基线版本 | v0.2.0（tag `v0.2.0`，CI 三 job 全绿） |
| 交付形态 | 源码 + CI 断言 + 文档（无真机执行条件，见 §5） |

### 1.1 原始需求复述

focusd 的公共基础设施价值取决于**被 Solaar / logiops / OpenLogi 等外设工具长期常驻消费**。
而锁屏/注销、`sway reload`、KWin 重载在真实桌面上天天发生；当前 focusd 在后两者下要么直接退出、要么卡在陈旧状态，常驻无从谈起。

本迭代按第一性原理只做一件事：**让 focusd 在会话生命周期内活得比 compositor 的抖动更久，并且在它「不知道」的时候诚实地承认不知道。**

### 1.2 关键产品决策（本 PRD 已裁定，需主理人/架构师确认）

| # | 决策 | 理由 |
|---|---|---|
| **D1** | **首次连接失败 vs 运行中断开，区别对待**：启动时首次连接失败（无 `WAYLAND_DISPLAY` / 协议不支持）→ **保持现有行为：明确报错退出**；运行时断开 → **重连，serve 永不退出** | 首次失败几乎总是配置错误（不在 Wayland 会话、指错 socket），重连无意义且会在 systemd 里挂一个永远修不好的僵尸服务。此区分同时**保住现有 CI 断言**（集成 job 的「无 compositor 时 serve 明确报错退出」与 smoke job）不被破坏 |
| **D2** | **重连/降级期间对外状态 = 无焦点**：`GetFocus()` 返回 `ss "" ""`，并发射一次 `FocusChanged("","")`，**不保留最后值** | 陈旧值比"未知"危险得多——消费方会拿旧 `app_id` 去应用**错误的**外设配置，用户看到的是"配置切错了"；而空串会让消费方回落到默认配置，与锁屏/桌面空白处的既有语义完全一致（`docs/dbus.md`：「空串 = 无焦点」）。**宁可承认不知道，不可编造** |
| **D3** | **重连期间不重新选择后端**；后端身份在进程生命周期内固定 | 运行中 compositor 换了种类（Sway→KWin）不是真实场景，重新探测会引入状态机复杂度与不可预期行为。重连只重试**同一个**后端 |
| **D4** | **重连日志分级抑制**：第 1 次断开 `warn!`；退避重试期间 `debug!`；退避档位每次提升或每 10 次重试才再 `warn!` 一次；恢复成功 `info!` 明确记录「已恢复，第 N 次重连」 | 注销/会话结束前后 focusd 会持续重连直到 systemd 停掉它，不抑制会刷爆 journald，掩盖真正的故障信息 |
| **D5** | **不新增 `Focus` 字段**；P0 内不改动 `GetFocus`/`FocusChanged` 签名 | 保持接口契约零变更，消费方（尚无真实下游）零迁移成本。是否对外暴露连接状态单独立项（R6，P1） |
| **D6** | **退避默认值**（建议）：初始 500ms，×2 指数递增，上限 30s，附加 ±20% 抖动；serve/watch **默认无限重试**（`FOCUSD_RECONNECT_MAX_ATTEMPTS=0` 表示无限） | 常驻 daemon 不应主动放弃；上限 30s 保证恢复延迟可控；抖动避免多个实例同步重试。CI 通过 env 调小至毫秒级以保证测试秒级完成 |

---

## 2. 产品目标（3 个，正交）

| # | 目标 | 衡量方式 |
|---|---|---|
| **G1 连续性** | compositor 中途重启 / Wayland display 断开后，focusd **不退出、自动重连、恢复后继续上报**，消费方无需重启自身 | CI 端到端：kill + 重启 headless Sway，serve 进程存活且恢复上报 |
| **G2 诚实性** | 任何 focusd 无法确认焦点的时刻（断连、后端降级），对外一律呈现为**无焦点空串**，绝不返回陈旧值 | CI：断连期间 `GetFocus` 断言为 `ss "" ""` |
| **G3 可诊断** | 每个后端的失效（KWin 脚本失效、GNOME 扩展消失）都能被**感知**，并以「结构化日志 + 明确降级行为 + 可操作提示」表达，而非静默陈旧 | 单测/mock 覆盖判定逻辑；真机清单覆盖恢复行为 |

---

## 3. 用户故事

| # | 用户故事 |
|---|---|
| **U1** | **作为**常驻消费 focusd 的外设工具 daemon（Solaar / OpenLogi 的 agent 线程），**我想**在用户 `sway reload`、compositor 崩溃重启之后继续收到 `FocusChanged` 而不必重启我自己，**以便**"按应用自动切换配置"在整个桌面会话期间始终有效，而不是用着用着就失灵。 |
| **U2** | **作为**外设工具开发者，**我想**在 focusd 与 compositor 断连期间从 `GetFocus()` 读到**空串**而不是上一次的陈旧 `app_id`，**以便**我能确定地回落到默认配置，而不是给用户的鼠标套用错误的 profile。 |
| **U3** | **作为**桌面用户（Sway / Hyprland / river / labwc），**我想**在我 `sway reload` 或锁屏解锁之后 focusd 依然照常工作，不需要我手工执行 `systemctl --user restart focusd`，**以便**我配置一次之后就彻底忘记它的存在。 |
| **U4** | **作为** KDE Plasma 用户，**我想**在 KWin 重载/重启之后，focusd 能自己发现脚本失效并重新拉起（或者至少明确告诉我"焦点已不可用，请重新启用 KWin 脚本"），**以便**我不会在毫无提示的情况下用着一套失灵的按应用配置。 |
| **U5** | **作为** GNOME 用户，**我想**在 Shell 重载或我不小心 disable 了扩展又 enable 回来之后，focusd 自动恢复上报，**以便**我不用为了一次 Shell 重载去重启 focusd。 |
| **U6** | **作为**打包者 / 发行版维护者，**我想** systemd unit 在 focusd 意外退出时能自动拉起且留下可诊断的日志，**以便**我可以放心把它作为依赖随外设工具一起分发。 |

---

## 4. 需求池

> 优先级：P0 = 必须做（本迭代不成立则无意义）；P1 = 应该做；P2 = 可做（有余力则做）。
> **验证方式**列是本 PRD 的硬约束：CI 环境为 `ubuntu-latest` 无显示，**只有 headless Sway 可跑真 compositor**，GNOME/KDE 真桌面不可。凡标"需真机"的项，必须登记进 `docs/MANUAL-TEST.md`，**不得假装被 CI 覆盖**。

| # | 需求 | 优先级 | 验收标准 | 验证方式 | 备注 |
|---|---|---|---|---|---|
| **R1** | **compositor 重启 / Wayland display 断开后自动重连**（wlroots 后端） | **P0** | ① `focusd serve --backend wlroots` 运行中 kill 掉 compositor：**进程不退出**（存活、不 panic、不 busy loop，CPU < 1%）；② 新 compositor 就绪后 ≤30s 内 `GetFocus` 重新返回真实 `app_id`，并再发射 `FocusChanged`；③ 重连成功后重新拿到全部既有 toplevel（不丢窗口）；④ **首次连接失败仍明确报错退出**（保持现有 CI 断言不被破坏）；⑤ 重连过程中 D-Bus 服务不中断（`org.focusd.Focus1` 始终可被 introspect） | **CI 可端到端**（headless Sway 可 kill + 重启）+ 单测 | 见 D1/D3。重连需**重读 `WAYLAND_DISPLAY`**（进程内环境变量是启动快照，socket inode 已变）；若实测 socket 名变化导致重连不成功，需增补 `$XDG_RUNTIME_DIR/wayland-*` 探测 —— 由架构师在 CI 实测确认并写入架构文档 |
| **R2** | **断连/降级期间对外状态语义 = 无焦点** | **P0** | ① 检测到断开后 ≤1s 内向通道推送 `Focus::default()`；② `busctl GetFocus` 返回 `ss "" ""`，且总线出现一次 `FocusChanged("","")`；③ 断连期间**不返回任何非空 app_id**；④ 重连恢复后推送真实快照（空→非空再发一次信号） | **CI 可**（busctl 断言 + 单测） | D2 决策。与 `docs/dbus.md` 既有「空串 = 无焦点」约定一致，消费方零改动。与已修复的 L1（焦点消失推 None）是同一条链路的延伸 |
| **R3** | **重连逻辑可注入 + 退避参数可配置**（可测试性硬要求） | **P0** | ① 提供可注入的连接/会话抽象（trait 或闭包工厂），测试可注入「前 N 次连接失败」「第 K 次 dispatch 返回 Err」「永远失败」的假实现；② 退避参数经 env 可配：`FOCUSD_RECONNECT_MIN_MS`（默认 500）/ `FOCUSD_RECONNECT_MAX_MS`（默认 30000）/ `FOCUSD_RECONNECT_MAX_ATTEMPTS`（默认 0 = 无限）；③ 单测覆盖：重连次数、退避序列、断连推 None、恢复后继续推送、达上限后行为、日志分级抑制（D4）；④ CI 单测总耗时 < 5s（env 调小退避） | **CI 单测 100%** | **这是本 PRD 对工程实现的硬性要求**：不接受"重连逻辑直接内联在 `run()` 里、无法单测"的实现。参照 `selector::ProbeContext` 已有的可注入范式 |
| **R4** | **KDE：KWin 脚本健康检测 + 自动重注册**（含不可行时的降级产品行为） | **P1** | ① KWin 脚本失效（KWin 重启 / 脚本被禁用）后 ≤30s 被感知，日志 `warn!` 且含可操作文案；② **若自动重注册可行**：自动 `loadScript` 成功、恢复上报、日志 `info!`；③ **若不可行或重载失败**：进入 degraded —— 对外上报无焦点（同 R2），并给出"请在 系统设置 → 窗口管理 → KWin 脚本 重新启用 focusd，或执行 `kpackagetool6 -i` 重装"的明确提示，**用户无需重启 focusd**；④ 检测周期可配（`FOCUSD_KWIN_HEALTH_MS`，默认 10s） | **CI 部分**（健康判定的纯逻辑单测 + mock bus name 消失/重现的契约测试）；**真机必测** | 现状是 `loop { thread::park() }` 永久沉睡，静默陈旧。**自动重注册是否真能生效（focusd 不重启的前提下）需真机 K 段确认**：若不可行，则本需求降级为「仅检测 + 明确提示」，README 已知限制第 2 条改写为"KWin 重启后 focusd 会报告无焦点并提示重新启用，无需重启 focusd" |
| **R5** | **GNOME 轮询链路自愈** | **P1** | ① 扩展被 disable / Shell 重载后，轮询失败累计达阈值（默认 3 次）即重建连接与 Proxy；② 扩展恢复后 ≤1 个轮询周期自动继续上报，**无需重启 focusd**；③ 失败期间对外上报无焦点（同 R2），日志 `warn!` 含"请 `gnome-extensions enable focusd@rayc2026.github.io`" | **CI 契约 mock**（mock 服务消失后重现）；**真机**（MANUAL-TEST G7 增补"无需重启 focusd"断言） | 现状：Proxy 长驻、失败只打 `debug!`，扩展消失后**永久静默陈旧**，与本迭代主题直接冲突。成本低于 R4，建议同批做 |
| **R6** | **对外暴露连接/重连状态** | **P1**（待确认） | ① `org.focusd.Focus1` 上新增**只读**状态查询，建议 `GetStatus() -> (s state, u32 generation)`，`state ∈ {connected, reconnecting, unavailable}`；② 不影响既有 `GetFocus`/`FocusChanged` 契约（老消费方零改动）；③ 语义同步进 `docs/dbus.md` 与 `docs/integration-guide.md` | **CI 可**（契约测试） | 排障价值高（用户抱怨"配置不切了"时能一眼区分"真无焦点"与"focusd 正在重连"），但引入新接口需单独设计。**待确认是否本迭代做**（见 §7-Q3）；不做则完全依赖 journald 日志 |
| **R7** | **wlroots 协议版本协商放宽** | **P2** | ① 按 registry 通告版本绑定 `1..=min(announced, 3)`，去掉硬编码 `3..=3`；② 在 headless Sway（v3）上行为无回归；③ 代码注释说明依赖事件的 since 版本（以 `wayland-protocols-wlr` 的 `EVT_*_SINCE` 常量核对），低版本下不使用 v3-only 能力 | **CI 部分**（绑定区间计算逻辑单测 + v3 回归）；**v1/v2 无法真机验证**（市面上几乎无此类 compositor） | 消除 README 已知限制第 1 条。收益偏防御性（focusd 只读 `app_id`/`title`/`state`/`closed`/`done`，这些自 v1 起存在），故列 P2 |
| **R8** | **systemd unit 兜底 + 文档收口** | **P2** | ① unit 增 `Restart=on-failure` + `RestartSec=2`；② 文档明确"仅兜底——重连失败必须有 ERROR 日志，不得靠自动重启掩盖"；③ README「已知限制」按本迭代结果更新（第 1/2/3 条相应改写或删除）；④ `docs/MANUAL-TEST.md` 增补：kill+restart compositor、锁屏→解锁信号序列、KWin 重载、GNOME 扩展重载的真机项；⑤ 若做 R6，同步 `docs/dbus.md` | 真机（人工清单） | 低成本收口项 |
| **R9** | **README「已知限制」与文档同步** | **P1** | ① 已知限制 4 条中，本迭代解决的第 2/3 条相应改写或删除，未解决的明确标注现状；② **`docs/MANUAL-TEST.md`** 增补本迭代全部真机项（见 §5 表）；③ 若 R6 纳入，同步 `docs/dbus.md` + `docs/integration-guide.md` | 真机（人工清单） | 与 R8 合并执行亦可；文档未同步不得发布 v0.3.0 |

---

## 5. 验证方式矩阵（CI vs 真机）

| 场景 | CI 能自动化？ | 手段 |
|---|---|---|
| 重连状态机（退避序列、次数、上限、日志抑制） | ✅ **完全可以** | R3 可注入连接器 + 假时钟/极小退避，纯单测 |
| 断连期间的对外状态语义（空串 + 信号） | ✅ **完全可以** | 注入假连接后立即断言通道收到 `Focus::default()`；集成层用 `busctl GetFocus` 断言 `ss "" ""` |
| **compositor 真实重启后恢复** | ✅ **可以**（wlroots 系专属） | headless Sway 是可控进程：CI 中 `kill` sway → 重启 sway → 重开 dummy-window → 断言 serve 存活且恢复上报。**分层断言**：L1（必过）进程存活 + `GetFocus` 为空串；L2（尽力）恢复后返回真实 app_id。若 L2 在 runner 上不稳定（socket 名变化 / 窗口重建时序），**必须降级为真机验证项并登记 MANUAL-TEST，不得因此关掉 gate 或改 `continue-on-error`** |
| D-Bus 服务在重连期间不中断 | ✅ 可以 | 重连循环期间持续 `busctl introspect org.focusd.Focus1` 成功 |
| KWin 脚本健康判定逻辑 | ✅ 可以（纯逻辑 + mock） | 注入假 bus 属主；契约测试里 own 再 drop 一个 mock `org.kde.KWin` |
| KWin 脚本**自动重注册是否真能生效** | ❌ **不能**，需真机 | MANUAL-TEST K 段重写（K9 当前写的是"需重启 focusd"，本迭代后应改为"无需重启"或"明确提示"） |
| GNOME Proxy 重建与自愈 | ✅ 契约 mock 可覆盖重建逻辑；❌ 真实 Shell 重载需真机 | MANUAL-TEST G 段增补 |
| 锁屏 / 解锁信号序列 | ❌ 不能（无真实会话与锁屏器） | MANUAL-TEST 增补（`docs/integration-guide.md` 已列"锁屏→解锁无异常风暴"，需落到清单） |
| 协议版本 v1/v2 兼容 | ❌ 不能 | 仅单测绑定区间计算；README 保留说明 |

**原则（沿用迭代二）**：CI 里每个后端/链路至少有"契约被 mock 验证"的测试；真桌面行为明确标注为人工步骤，不假装自动化覆盖。

---

## 6. 明确排除（不在本次范围）

- ❌ **GNOME / KDE 真机人工验证的执行**（用户侧按 `docs/MANUAL-TEST.md` 执行，本迭代只负责把清单写实）
- ❌ **社区对接 OpenLogi / Solaar 上游**（路线图阶段五，接口就绪后另行推进）
- ❌ **权限模型设计**（README 已知限制第 4 条，需 compositor 侧能力配合，非本迭代主题）
- ❌ **新增后端（COSMIC 等）**、❌ **`Focus` 结构加字段**、❌ **D-Bus 命名空间迁移到 `io.github.rayc2026.*`**
- ❌ **多 seat / 多 display 同时监听**（单一会话单一后端，D3 已定）

---

## 7. 待确认问题（需架构师 / 主理人裁定）

| # | 问题 | PM 倾向 |
|---|---|---|
| **Q1** | **重连期间的状态语义**（D2）是否采纳"上报无焦点"而非"保留最后值"？这会让消费方在断连时回落到默认配置 | **强烈倾向"上报无焦点"**——陈旧值是错误行为，空串是既有约定且零改动。请确认 |
| **Q2** | **退避策略默认值**（D6）：500ms 起 / ×2 / 上限 30s / ±20% 抖动 / serve 无限重试，是否可接受？ | 建议按此执行，全部做成 env 可配以便调参 |
| **Q3** | **是否在本次迭代对外暴露重连状态**（R6 `GetStatus`），还是只依赖 journald 日志？ | 倾向 **R6 列 P1 但不阻塞 P0**；若 P0 工期紧张可整体推迟到迭代四。请裁定：做 / 不做 |
| **Q4** | **KWin 自动重注册（R4 上半）是否真能生效？** 需真机确认（KWin 重启后，focusd 不重启直接再 `loadScript` 是否会让脚本重新订阅 `windowActivated`） | 无法预先判断。**产品要求**：无论能否自动恢复，都必须消除"静默陈旧"——能恢复就恢复，不能就明确提示用户，且两种行为都要写进文档 |
| **Q5** | **重连时 `WAYLAND_DISPLAY` 已失效（socket 名变化）如何处理？** 是否需要扫描 `$XDG_RUNTIME_DIR/wayland-*`？ | 建议：先重读环境变量；若 CI 的 kill+restart 实测不通过，再由架构师决定加 socket 探测。请架构师在 CI 实测后给出结论 |
| **Q6** | **`watch` 子命令是否也重连？** （当前 CLI 调试用，退出策略可不同于 serve） | 倾向**共用同一套 supervisor、同样无限重试**（用户 Ctrl-C 退出即可），避免两套语义。请确认 |
| **Q7** | **首次连接失败报错退出（D1）与 R1 的兼容性**：现有集成 job「无 compositor 时 serve 明确报错退出」断言是否保留？ | **保留**——D1 正是为保住它而设计。请工程师不要误删该断言 |
| **Q8** | **R5（GNOME 自愈）** 主理人原始需求池未列，是否纳入？ | 建议**纳入 P1**：与迭代主题同源（静默陈旧），成本低于 R4，且 MANUAL-TEST 的 G7 现有描述（"重新 enable 后恢复"）疑似与代码不符，本迭代应一并查实 |
