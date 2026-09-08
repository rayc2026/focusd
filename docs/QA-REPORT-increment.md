# focusd 迭代二 QA 测试报告

> QA：严过关 ｜ 日期：2026-09-08
> 验证对象：远端 main `9451478`（对应 CI run **34183110985**）
> 验证方式：独立核查（fresh eyes）——只信证据，不信工程师自述。本机无 Rust 工具链，编译/测试证据全部取自 GitHub Actions 实际日志与仓库 blob 比对。

---

## Summary

- **验证项：6 大项 / 21 个文件逐项核查 → 全部通过**
- **CI run 34183110985**：build / integration / smoke 三 job 全部 `success`，无失败 step；`cargo clippy --all-targets -- -D warnings` 通过；11 个测试（9 单测 + 2 契约）全绿
- **路由判定：NoOne**（无阻塞源码 Bug，无测试代码 Bug；2 项遗留问题见 §4）

---

## 1. 假阳性修复的真实性（最高优先项）—— ✅ 独立证实

### 1a. STATE_ACTIVATED=2 与协议一致 —— 证实

- 代码：`src/backend/wlroots.rs:25` `const STATE_ACTIVATED: u32 = 2;`，与 zwlr-foreign-toplevel-management-unstable-v1 协议枚举（maximized=0 / minimized=1 / activated=2）一致。
- **独立交叉验证（不依赖代码自述）**：CI integration 日志中新增的 debug 输出打印了 compositor 发来的 state 原始字节，实际观测：
  - 被激活窗口：`state 原始字节: [02, 00, 00, 00]` → `activated=true`
  - 未激活窗口：`state 原始字节: []` → `activated=false`
  
  真实 compositor（headless Sway）对激活窗口发的正是 `2`——若仍是旧值 1，`activated` 永远匹配不上，与工程师描述的根因吻合。原始字节观测是最强证据。

### 1b. watch 断言分流 —— 已落实

`ci.yml:115`：`timeout 25 env RUST_LOG=debug ./target/debug/focusd watch > /tmp/focusd.out 2> /tmp/focusd.err &`，断言（`ci.yml:132`）只 grep `/tmp/focusd.out`。日志行（stderr）**不可能**再污染断言文件，假绿路径已被结构性消除。

### 1c. run 34183110985 watch 段输出真实焦点数据 —— 证实

stdout（`/tmp/focusd.out`）实际内容（日志摘录）：

```
focusd.win2	two
focusd.win1	one
focusd.win2	two
```

3 行 = 与 `swaymsg focus right` → `focus left` 的切换序列精确对应，且无同值重发（去重生效）。这是**真实焦点数据**，不是日志污染。

---

## 2. D-Bus serve 断言质量 —— ✅ 通过

| 断言 | 证据（run 34183110985 日志摘录） |
|---|---|
| GetFocus 真读 serve 模式 | serve 在 `dbus-run-session` 内启动后，`busctl --user call org.focusd.Focus1 ... GetFocus` 第 1 次尝试即返回 `ss "focusd.win2" "two"`；serve 日志同时显示 `D-Bus 服务就绪: org.focusd.Focus1 @ /org/focusd/Focus1`，且状态由 serve 主循环写入（`dedup: 快照变化 -> Focus { app_id: Some("focusd.win2") ... }`） |
| FocusChanged 被 monitor 捕获 | `busctl --user monitor` 完整捕获信号消息：`Interface=org.focusd.Focus1  Member=FocusChanged`，消息体 `STRING "focusd.win1"; STRING "one"` —— 与 `swaymsg focus left` 的实际切换一致，且与 serve 日志 `serve: FocusChanged 已发射: focusd.win1 / one` 互为印证 |
| None ↔ 空串约定三处一致 | **代码**：`backend/mod.rs` `empty_to_none()`（KWin/GNOME 接收侧 ""→None）+ `dbus/mod.rs` `get_focus` 的 `unwrap_or_default()`（None→""）；**文档**：`docs/dbus.md`「语义约定」明示空串=无焦点；**测试**：`tests/kde_kwin_report.rs:61-64` 断言 `Report("","")` → `app_id=None, title=None`。三处一致 |
| 无 compositor 不挂死 | `env -u WAYLAND_DISPLAY serve` 立即以可操作错误退出：`Error: 后端 wlroots 探测失败: WAYLAND_DISPLAY 未设置…`，`VERIFIED: 无 compositor 时 serve 明确报错退出` |

---

## 3. 单元与契约测试质量 —— ✅ 通过（非空转）

CI 实际执行的 11 个测试及性质评估：

**9 个单测**（`dbus-run-session cargo test` 内，全绿）：
- selector 6 个：`FakeEnv` 注入假环境（变量表 + bus 属主列表），覆盖 wlroots 会话选中 wlroots / KDE 会话选中 kde / 全空环境聚合三条原因 / 未知后端列出可用项 / `--backend` 强校验（含 wlroots 会话指定 gnome 必须失败）/ 注册表顺序。测的是**真实选择逻辑**，非 mock 空转。
- `dedup_只放行真正变化`（主循环去重语义）、`empty_to_none_空串映射为none`、`load_script_在无_kwin_环境报可操作错误`（CI 中 org.kde.KWin 不在线，走真实失败路径并断言文案含 kpackagetool6 指引）。

**2 个契约测试**（走真实生产路径）：
- `kde_kwin_report.rs`：直接调用**生产组装入口** `dbus::start_serve`（非测试专用桩），真实 zbus client 按 KWin 脚本的精确契约调 `Report`，断言 3 次推送 → 通道 3 事件 → Dedup 后 2 快照 + 空串→None。
- `gnome_poll.rs`：mock 扩展服务持有 `org.focusd.Gnome1`，被测对象是**真实的 `GnomeBackend.run` 轮询循环**（真实 client 代码打 mock server，方向正确），断言轮询序列 firefox→foot 且同值去重。
- selector 探测逻辑环境注入覆盖三种后端与降级路径：已覆盖（见上）。

---

## 4. KDE / GNOME 组件 —— ✅ 通过

**KWin Script**（`packaging/kde/org.focusd.kwin/`）：
- `main.js` 契约与 Rust 侧逐项一致：`SERVICE=org.focusd.Focus1`、`PATH=/org/focusd/Focus1`、`IFACE=org.focusd.Focus1.Kwin`、`Report(resourceClass, caption)`——与 `src/backend/kde.rs` 常量及 `KwinReportIface` 签名精确匹配；`windowActivated` / `captionChanged`（防御式「存在即连接」）链路符合架构文档 §5.1；`include_str!` 内嵌同一文件，自动加载与 kpackagetool6 手动安装路径不会漂移。
- `metadata.json`：`KPackageStructure: KWin/Script`、`ServiceTypes`、`X-Plasma-API: javascript`、`X-Plasma-MainScript: code/main.js` 格式正确。

**GNOME 扩展**（`packaging/gnome/focusd@rayc2026.github.io/`）：
- ESM `import Gio from 'gi://Gio'`（Shell 45+ 正确写法）；`own_name('org.focusd.Gnome1')` + `export('/org/focusd/Gnome')` + `GetFocus()` 返回 `[wm_class, title]`、无焦点返回 `['','']`——与 `GnomeBackend`（`GNOME_BUS_NAME/GNOME_PATH/GNOME_IFACE`、`(String,String)` 返回类型、空串→None）及 `docs/dbus.md` 契约一致；`disable()` 正确 unexport + unown。
- `metadata.json`：uuid `focusd@rayc2026.github.io` 格式正确（含 `@`，与目录名一致）；shell-version 45–48 合理。
- `FOCUSD_POLL_MS` 默认 250、clamp 20ms–5s：`gnome.rs` `poll_interval()` 与 `docs/dbus.md` 描述一致。

---

## 5. CI workflow 稳健性 —— ✅ 通过

- **无 `continue-on-error`**：`ci.yml` 全文无该字段（grep 证实），integration `needs: build`，脚本内 `set -e`，失败即红——已由 run 34183110985 三 job 全 success、无失败 step 佐证（此前 `9603e3c` 的真实失败也确实红了，门禁真实生效）。
- 单测/契约包在 `dbus-run-session` 内（zbus 需要会话总线），结构合理。
- serve 段：GetFocus 轮询重试上限 ~10s；monitor 用 `timeout 15` 包裹并显式 kill；serve 显式 kill。窗口进程未显式 kill，但 GitHub runner 的 orphan cleanup 已接管（日志可见 `Terminate orphan process: dbus-run-session / sway / dummy-window`），不会累积僵尸。
- 注：本轮共 4 个 run（工程师 gh API 逐文件兜底所致），最终 run 34183110985 全绿，无遗留红 CI。

---

## 6. 文档一致性 —— ✅ 通过

- `docs/dbus.md` 的 bus name / 路径 / 两个接口 XML 签名 / GNOME 侧契约 / 迁移说明，与 `src/dbus/mod.rs` 常量、zbus 宏接口、`gnome.rs` 完全一致。
- `docs/MANUAL-TEST.md` 清单（K1–K11 / G1–G9 / W1–W2 / S1–S3）步骤具体、预期可判定、失败处置明确（「任何一项失败不得标记已验证」），可操作。
- `packaging/systemd/focusd.service` 含 `PartOf`/`After=graphical-session.target`、`WantedBy=graphical-session.target`，满足 PRD 验收要求。
- README 含「如何新增一个后端（以 COSMIC 为例）」「后端语义差异」「D-Bus 接口」「测试策略」各节，满足 T05 验收（外部开发者可据此说出新增后端需要实现什么）。

---

## 7. 内容一致性核验（审查对象 = CI 验证对象）

远端 main 最终提交为 `9451478`（gh API 逐文件兜底产生 4 个同消息提交），本地 HEAD `de6333a` 不在远端历史中。已对**全部 21 个关键文件**（ci.yml、src/ 全部、tests/ 全部、packaging/ 全部、docs/、Cargo.toml、README、examples）做本地 blob sha ↔ 远端 contents sha 比对：**全部一致**。本地审查结论对远端 CI 验证对象有效。建议后续将本地仓库同步到远端 main，避免双历史混乱。

---

## 发现的问题与遗留问题（均不阻塞本次迭代验收）

### L1（低severity，建议下迭代修复）：wlroots 后端不上报「无焦点」
- 位置：`src/backend/wlroots.rs:56-65` `State::emit_if_changed`
- 问题：当焦点消失（最后一个窗口关闭、`current()` 返回 None）时，函数更新内部 `last` 但**不向 channel 发送任何事件**（`if let Some(f) = &cur` 跳过 None）。后果：serve 模式下 GetFocus 将**永远返回最后一个焦点窗口**（陈旧值），与 `docs/dbus.md` 对消费方的契约「空串 = 当前无焦点窗口」不一致。KDE（Report 空串）与 GNOME（空 wm_class）路径均可上报无焦点，唯独 wlroots 路径不能；CI 与 MANUAL-TEST W 段均未覆盖此场景。
- 建议修复：`cur == None && last != None` 时也推送 `Focus { app_id: None, title: None }`（serve 侧写 state None、watch 打印 `-	-`），并在 MANUAL-TEST W 段补一条「关闭最后一个窗口 → GetFocus 返回空串」；或退而在文档中将 wlroots 的该行为登记为已知限制。
- 定级依据：不违反 PRD 本迭代任何验收标准（GetFocus「返回当前焦点」、FocusChanged「与实际切换一致」均已满足；MVP watch 语义本就是「只在变化时输出」），日常使用中焦点消失场景较少见，故为遗留问题而非阻塞 Bug。

### L2（流程提醒）：远端 main 存在 4 个同消息 T05 提交
gh API 逐文件兜底的副产物。CI 无碍，但建议团队后续约定：兜底推送后用一次 squash/空提交收口，或在本地 rebase 对齐，避免历史混乱（见 §7）。

---

## 路由判定

**NoOne —— 全部通过。**
- 假阳性根因（activated=1→2）修复真实性：已用 CI 日志中 compositor 原始 state 字节 `[02,00,00,00]` 独立证实；
- watch 断言分流：结构性消除日志污染路径；
- serve GetFocus / FocusChanged 断言：真实数据、真实信号，证据链完整；
- 测试 11/11 绿、三 job 全绿、无遗留红 CI；
- 无需转工程师修复，无测试代码需 QA 修复；遗留问题 L1 建议登记至下迭代待办。
