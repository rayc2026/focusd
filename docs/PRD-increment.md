# focusd 增量 PRD（迭代二）

> **性质说明**：本 PRD 是**增量文档**，只描述本次迭代的变更部分。基线是 README 中已实现并经 CI 集成测试验证的 MVP（wlroots 后端 + `probe`/`watch` CLI + Backend trait 架构）。基线功能不在本文重复。
>
> 日期：2026-09-08 ｜ 作者：产品经理 许清楚

---

## 1. 本次迭代目标

MVP 已回答了核心假设——"能否稳定、及时、低开销地拿到焦点 app_id"。本次迭代按第一性原理推进"项目完成"的定义：

1. **焦点感知层可被外部程序消费**：加 D-Bus 接口，让 Solaar/OpenLogi/logiops 等外部工具不需要 fork/解析 focusd 的 stdout 就能拿到焦点（这是本项目的公共基础设施价值所在）。
2. **覆盖面扩展**：wlroots 系只覆盖部分桌面。补齐 KDE（KWin Script）与 GNOME（Shell Extension）后端，让主流桌面用户也能用。
3. **质量门禁固化**：CI 集成测试目前 `continue-on-error: true`，必须转正为硬性 gate，防止后续重构（尤其是加 D-Bus 与多后端时）悄悄破坏核心链路。
4. **可部署**：提供自启动方式（systemd user unit），并补充架构文档，让外部开发者能看懂 Backend trait 如何扩展。

**明确不在本次范围**：README 路线图阶段五"对接 OpenLogi/Solaar 上游"——属社区协作，接口就绪后另行推进。

---

## 2. 用户故事

| # | 用户故事 |
|---|---|
| U1 | **作为**外设工具开发者（如 logiops/Solaar 贡献者），**我想**通过 D-Bus 调用 `org.focusd.Focus1.GetFocus()` 一次拿到当前焦点应用的 `app_id` 和 `title`，**以便**在我的守护进程里实现"按应用自动切换配置"，而无需依赖 X11 或解析子进程输出。 |
| U2 | **作为**外设工具开发者，**我想**订阅 D-Bus 的 `FocusChanged(app_id, title)` 信号而不是轮询 `GetFocus()`，**以便**在我的工具中零延迟响应焦点切换，且不产生持续轮询开销。 |
| U3 | **作为** KDE Plasma 桌面用户，**我想** focusd 在 KDE 上自动可用（通过 KWin Script 拿焦点），**以便**我在 Wayland 会话下也能享受按应用切换外设配置。 |
| U4 | **作为** GNOME 桌面用户，**我想**安装一个 Shell 扩展后 focusd 就能拿到焦点信息，**以便**我无需放弃 Wayland 安全模型即可使用依赖焦点感知的工具。 |
| U5 | **作为**桌面用户，**我想** focusd 作为 systemd user service 开机自启、随会话常驻，**以便**我配置一次后就不用再管它。 |
| U6 | **作为**想给 focusd 贡献新后端的外部开发者，**我想** README 里有清晰的 Backend trait 扩展指南和后端选择机制说明，**以便**我知道加一个 COSMIC/其他后端需要实现什么。 |

---

## 3. 需求池

| 需求 | 优先级 | 验收标准 | 备注 |
|---|---|---|---|
| **D-Bus 接口 `org.focusd.Focus1`**：Session bus 上提供 `GetFocus()` 方法（返回 app_id + title）与 `FocusChanged` 信号；daemon 模式（如 `focusd serve`）同时跑 watch 逻辑与 D-Bus 服务 | P0 | `busctl --user introspect` 可见接口；`busctl --user call` 调用 `GetFocus()` 返回当前焦点；`gdbus monitor` 能收到 `FocusChanged` 信号且与实际切换一致；无 compositor 时 daemon 报错退出而非挂死 | 引入 `zbus`（纯 Rust，无 libdbus 依赖）。信号只在焦点真正变化时发（复用现有去重逻辑）。`Focus` 结构保持两字段（app_id/title），不扩字段 |
| **多后端探测与选择机制**：启动时按 wlroots → KDE → GNOME 顺序自动探测可用后端；无法探测到任何后端时报错并列出原因 | P0 | 在 wlroots 会话自动选中 wlroots 后端；指定 `--backend` 时强校验并给出明确错误；三个会话环境各自行为有单元测试覆盖 | 探测逻辑：先看 `WAYLAND_DISPLAY` + 协议支持，再查 KDE/GNOME 特征（`XDG_CURRENT_DESKTOP` / D-Bus 名称）。Backend trait 不变，新增 `id()`/`probe()` 元数据 |
| **CI integration gate 转正**：集成测试 job 从 `continue-on-error: true` 改为失败即红 | P0 | 集成测试失败时 CI 整体红；主分支 CI 三 job 全绿 | 在加 D-Bus 之前转正，先锁住现有核心链路。如后续 job 依赖 headless Sway 环境不稳定，允许增加 1 次重试，但不允许静默跳过 |
| **KDE KWin Script 后端**：安装/加载一个 KWin Script，通过脚本经 D-Bus 把焦点窗口（resourceName/desktopFile 名）推回 focusd | P1 | 在真机 KDE Plasma 上：切窗口后 `focusd watch` 正确输出 app_id；脚本加载失败时给出用户可操作的提示 | 需用户启用脚本——CLI 应提供加载指引（如 `kpackagetool6 --type=KWin/Script` 安装命令）。**CI 无法自动化验证真桌面**，测试策略见 §4 |
| **GNOME Shell Extension 后端**：一个最小 Shell 扩展通过 D-Bus（如自建 `org.gnome.Shell.Extensions.focusd`）暴露焦点窗口的 WM_CLASS / app_id | P1 | 在真机 GNOME 上：启用扩展后 `focusd watch` 正确输出 app_id | GNOME 上拿到的是 WM_CLASS 而非 wlroots 语义的 app_id，文档需说明字段语义差异。**CI 无法自动化验证**，见 §4 |
| **`--backend <id>` CLI 参数**：`probe` 与 `serve` 支持手动指定后端 | P1 | `focusd serve --backend wlroots` 在 wlroots 会话正常工作；指定不存在的后端名时报错并列出可用项 | 自动探测之外的逃生舱口 |
| **README 架构文档**：补 Backend trait 扩展指南、后端选择机制、D-Bus 接口文档（含 signal 签名）、KDE/GNOME 安装指引、测试策略说明 | P2 | 外部开发者按文档能说明白"如何新增一个后端"；D-Bus 接口签名与实现一致 | D-Bus 接口文档也可单独放 `docs/dbus.md`，README 链接过去 |
| **systemd user unit + 桌面集成**：提供 `focusd.service`（user unit），随会话自启；可选 `.desktop` 文件 | P2 | `systemctl --user enable --now focusd` 后 daemon 常驻并注册 D-Bus 名称；失败时有日志 | unit 中注明依赖 `graphical-session.target` |

---

## 4. 各后端的 CI 验证能力（测试策略边界）

| 后端 | CI 能否自动化验证 | 策略 |
|---|---|---|
| wlroots | ✅ **可以** | 维持现有 headless Sway + dummy-window 集成测试，作为硬性 gate；后续为 `serve` 模式补"headless Sway + busctl 调用 GetFocus / 监听 FocusChanged"的集成断言 |
| KDE KWin Script | ❌ 不能（无显示 runner 跑不了真 Plasma/KWin） | 单元测试 mock KWin D-Bus 返回 + 真机人工验证清单（写进 PR 模板/README）；标注"真机待验证" |
| GNOME Shell Extension | ❌ 不能 | 同上：单元测试 mock 扩展的 D-Bus 接口 + 真机人工验证清单 |
| 探测/选择逻辑 | ✅ 可以 | 纯环境变量/D-Bus 名称判断，注入假环境即可单测 |

原则：**CI 里每个后端至少有"接口契约被 mock 验证"的测试；真桌面验证明确标注为人工步骤，不假装自动化覆盖了。**

---

## 5. 待确认问题

1. **D-Bus 名称归属**：`org.focusd.Focus1` 是否需要申请 fd.o namespaced D-Bus 名称（如 `io.github.rayc2026.focusd`）以避免未来冲突？建议本次先用 `org.focusd.Focus1`（简单），文档中留迁移说明。
2. **`serve` 还是复用 `watch`**：D-Bus daemon 是新增子命令 `focusd serve`，还是让 `watch --dbus` 兼任？倾向新增 `serve`（CLI 语义更干净），待架构师确认。
3. **KDE 后端的依赖方向**：KWin Script 需要 focusd 反向作为 D-Bus client 接收推送，还是 focusd 主动调用 KWin 查询？影响 zbus 的 client/server 双角色设计，待架构师定。
4. **协议版本协商**：README 已知限制中"协议版本固定绑定 3"是否在本次顺带修掉？建议不在本次范围（避免与多后端工作纠缠），仅登记。
5. **异步运行时**：引入 zbus 后是否顺带引入 tokio？若 zbus 的 sync API 够用则避免双运行时，待架构师评估。
