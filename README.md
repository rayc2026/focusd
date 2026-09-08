# focusd

[![CI](https://github.com/rayc2026/focusd/actions/workflows/ci.yml/badge.svg)](https://github.com/rayc2026/focusd/actions/workflows/ci.yml)

Wayland 下的**焦点应用探测**守护进程 —— 回答一个本该有标准答案、但在 Wayland 上至今没有的问题：

> 现在用户正在用哪个应用？

## 为什么需要它

在 X11 上，任何程序都能查询 `_NET_ACTIVE_WINDOW` 拿到焦点窗口。Wayland 出于安全考虑**故意不提供这个能力**，结果就是所有依赖「当前活跃应用」的功能全线失效：

- 鼠标/键盘的**按应用自动切换配置**（Solaar、logiops、OpenLogi 在 Linux 上均受影响）
- 密码管理器的 AutoType（KeePassXC）
- 宏工具、窗口规则、时间追踪

### 空白验证结果（2026-09-07 核实）

| 方案 | 状态 |
|---|---|
| XDG Desktop Portal | **没有**获取焦点窗口的接口 |
| `ext-foreign-toplevel-list-v1`（标准协议） | 仍在 **staging（实验）**阶段；wlroots / COSMIC 已实现，**GNOME 与 KDE 均未实现** |
| KDE 官方态度 | `bugs.kde.org/483227` 状态为 **RESOLVED NOT A BUG** —— KDE 明确拒绝实现该协议，倾向于另做 password manager API |
| GNOME 官方态度 | 未见实现计划 |

**这不是「暂时没人做」，而是标准路径被否决了**，因此第三方适配层会长期有存在价值。

各 compositor 的可用路径：

| Compositor | 协议 / 机制 | 成熟度 |
|---|---|---|
| Sway / Hyprland / river / labwc（wlroots 系） | `zwlr_foreign_toplevel_manager_v1` | 稳定，广泛实现 |
| KDE Plasma | KWin Script → D-Bus | 可行，需用户启用脚本 |
| GNOME | Shell Extension → D-Bus | 可行，需用户安装扩展 |
| COSMIC | 已原生支持 `ext-foreign-toplevel-list-v1` | 新 |

## 当前状态

- ✅ **wlroots 后端**（`zwlr-foreign-toplevel-management-v1`）：实时捕获焦点变化
- ✅ **D-Bus 接口 `org.focusd.Focus1`**：`GetFocus()` 方法 + `FocusChanged` 信号（`focusd serve`）
- ✅ **KDE 后端**：KWin Script 自动加载（或 `kpackagetool6` 手动安装）→ D-Bus 推送
- ✅ **GNOME 后端**：Shell 扩展（own `org.focusd.Gnome1`）→ focusd 轮询
- ✅ **多后端自动探测**：`wlroots → kde → gnome`，`--backend` 手动指定 + 强校验
- ✅ **systemd user unit**：随图形会话自启（`packaging/systemd/focusd.service`）
- ☐ 对接 OpenLogi / Solaar 上游（接口就绪，属社区协作）

### 已验证 / 未验证

| 项目 | 状态 | 验证方式 |
|---|---|---|
| 代码可编译 + `cargo clippy -D warnings` 零告警 | ✅ | CI（硬性 gate） |
| 单测（选择器三会话场景 / Dedup / 空串约定） | ✅ | CI `dbus-run-session cargo test` |
| wlroots 焦点切换捕获 | ✅ | CI 集成：headless Sway + dummy-window，**断言真实 stdout**（曾发现并修复 activated 枚举值错误导致的假绿） |
| `serve` + D-Bus 契约 | ✅ | CI 集成：`busctl call GetFocus` 返回焦点 app_id；`busctl monitor` 捕获 `FocusChanged`；无 compositor 时明确报错退出 |
| KDE KWin Script | ☐ 真机待验证 | CI 覆盖 Kwin.Report 契约 mock（`tests/kde_kwin_report.rs`）；真机清单见 `docs/MANUAL-TEST.md` |
| GNOME Shell Extension | ☐ 真机待验证 | CI 覆盖轮询契约 mock（`tests/gnome_poll.rs`）；真机清单见 `docs/MANUAL-TEST.md` |

## 环境要求

**只能在 Linux 上构建和运行。** Windows / macOS 上 `cargo build` 会失败（依赖 libwayland-client），这是预期行为。

构建依赖：
```bash
# Debian / Ubuntu
sudo apt install libwayland-dev pkg-config build-essential

# Fedora
sudo dnf install wayland-devel pkgconf-pie gcc

# Arch
sudo pacman -S wayland pkgconf base-devel
```

运行要求：**Wayland 会话**。后端按 wlroots → kde → gnome 自动探测；
三个后端都不可用时 `focusd probe` 会列出每个后端的具体原因。

## 快速上手

```bash
cargo build --release

# 1) 探测环境（列出所有后端及各自探测结果）
./target/release/focusd probe

# 2a) CLI 监听（切换窗口观察输出）
./target/release/focusd watch
./target/release/focusd watch --format json   # JSON 输出

# 2b) D-Bus 服务模式（外部程序消费）
./target/release/focusd serve
busctl --user call org.focusd.Focus1 /org/focusd/Focus1 org.focusd.Focus1 GetFocus
gdbus monitor --session --dest org.focusd.Focus1 --object-path /org/focusd/Focus1
```

预期输出（切到 Firefox 再切到终端）：
```
firefox	Mozilla Firefox
foot	~
```

### GNOME 用户：安装 Shell 扩展

```bash
mkdir -p ~/.local/share/gnome-shell/extensions
cp -r packaging/gnome/focusd@rayc2026.github.io ~/.local/share/gnome-shell/extensions/
# 注销重登（Wayland 下无法热重载 Shell），然后：
gnome-extensions enable focusd@rayc2026.github.io
```

### KDE 用户

无需操作——`focusd watch/serve` 启动时会自动通过
`org.kde.kwin.Scripting.loadScript` 加载脚本（KWin 重启后需重启 focusd）。
也可持久安装：`kpackagetool6 --type=KWin/Script -i packaging/kde/org.focusd.kwin`，
再在 系统设置 → 窗口管理 → KWin 脚本 中启用 focusd。

### 开机自启（systemd user unit）

```bash
cp packaging/systemd/focusd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now focusd
journalctl --user -u focusd -f   # 看日志
```

unit 依赖 `graphical-session.target`，随图形会话启停。

## D-Bus 接口

| 接口 | 成员 | 说明 |
|---|---|---|
| `org.focusd.Focus1` @ `/org/focusd/Focus1` | `GetFocus() -> (s app_id, s title)` | 当前焦点快照；无焦点为空串 |
| 同上 | signal `FocusChanged(s app_id, s title)` | 仅在焦点真正变化（去重后）时发射 |
| 同上 | `Report(s app_id, s title)`（`org.focusd.Focus1.Kwin`） | KWin 脚本推送入口 |

完整 XML 签名、busctl/gdbus 用例、空串与 None 的约定、
未来迁移 `io.github.rayc2026.focusd` 的说明：见 **[docs/dbus.md](docs/dbus.md)**。

## 架构与扩展指南

### 架构一页图

```
Wayland 事件 ──► WlrootsBackend.run(tx) ─┐
KWin callDBus ─► (KwinReportIface) ──────┼─► mpsc channel ─► serve/watch 主循环
250ms 轮询 ────► GnomeBackend.run(tx) ───┘                    │ 去重（Dedup）
                                              ┌───────────────┴───────────────┐
                                              │ Arc<RwLock<Option<Focus>>>     │
                                              │ zbus ObjectServer:             │
                                              │   GetFocus / FocusChanged      │
                                              └───────────────────────────────┘
```

- 所有后端把焦点快照推入**同一条 channel**；去重只在主循环一处（`backend::Dedup`），
  保证 `FocusChanged` 不重复发射。
- serve 主循环「先写状态、后发信号」；zbus ObjectServer 在自己的内部线程应答 GetFocus。
- 线程模型：后端线程（阻塞 `run()`）→ mpsc → serve 主线程独占写状态。

### 如何新增一个后端（以 COSMIC 为例）

1. **新建 `src/backend/cosmic.rs`**，实现 `Backend` trait 四个方法：
   - `id()` —— 机器可读标识（`"cosmic"`），用于 `--backend` 与探测顺序；
   - `name()` —— 人可读名称；
   - `probe()` —— 轻量探测，**`Err` 文案必须可操作**（指明缺什么、怎么装；
     参照 `selector::probe_gnome` 的安装指引写法）；
   - `run(&self, tx: mpsc::Sender<Focus>)` —— 阻塞事件循环，把每个焦点快照
     推进 channel。**同值重发没关系**——去重由主循环保证，后端不必自己实现。
2. **在 `src/backend/selector.rs` 的注册表登记一行**：
   `Entry { id: "cosmic", probe: probe_cosmic, make: || Box::new(CosmicBackend) }`，
   位置即自动探测优先级。探测逻辑写成接受 `&dyn ProbeContext` 的**纯函数**
   （环境变量 + bus name 查询可注入），并在 `selector.rs` 的 `#[cfg(test)]`
   里加会话场景单测（CI 无真桌面，这是唯一能自动化验证探测逻辑的方式）。
3. **契约 mock 测试**（`tests/`）：无论后端事件模型是推送还是轮询，
   CI 里用 zbus 起临时服务模拟对端，验证 D-Bus 契约与通道行为——
   参照 `tests/kde_kwin_report.rs` / `tests/gnome_poll.rs`。
4. **真机验证项登记 `docs/MANUAL-TEST.md`**；涉及桌面侧组件（脚本/扩展）的
   放 `packaging/`，并保持与代码内嵌副本一致（参照 KWin 脚本的 `include_str!`）。
5. `Focus` 结构**不要加字段**：只承诺所有后端都能拿到的最小信息集
   （GNOME 拿不到 PID/几何，现在承诺了就收不回来）。

### 后端语义差异

| 后端 | app_id 来源 | 事件模型 |
|---|---|---|
| wlroots | Wayland `app_id` | 协议推送 |
| kde | KWin `resourceClass` | KWin Script `callDBus` 推送（实时） |
| gnome | **WM_CLASS**（X11 语义，与 Wayland app_id 不严格相等） | 轮询 `GetFocus()`（`FOCUSD_POLL_MS`，默认 250ms，clamp 20ms–5s） |

## Windows 用户：WSL2 + 嵌套 Sway（无需服务器或虚拟机）

这是最低成本的验证路径。**原理**：WSLg 虽然跑的是 Weston（不支持 wlroots 专有协议），但 wlroots 原生支持**嵌套运行**——可以在 Weston 里再跑一个 Sway，Sway 会创建自己的 Wayland socket 并实现 `zwlr_foreign_toplevel_manager_v1`。

> 参考：Purism 官方博客用 `WLR_BACKENDS=wayland` 嵌套运行 phoc（同为 wlroots 系）；
> `Giraut/nest_sway` 亦确认 "wlroots can already run nested natively without any modification"。

```bash
# 1) Windows PowerShell（管理员），装 WSL2 + Ubuntu
wsl --install -d Ubuntu

# 2) 进入 WSL，装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 3) 装构建依赖与 Sway
sudo apt update
sudo apt install -y libwayland-dev pkg-config build-essential sway foot

# 4) 把项目复制到 WSL 本地目录（不要放 /mnt/e 下，跨文件系统编译极慢）
cp -r /mnt/e/WorkBuddy*/2026-*/focusd ~/focusd && cd ~/focusd

# 5) 在 WSLg 终端里启动嵌套 Sway
export WLR_BACKENDS=wayland
sway

# 6) 在 Sway 会话里开终端（默认 Win+Enter），运行
cargo run -- probe   # 确认 WAYLAND_DISPLAY 指向 Sway 而非 Weston
cargo run -- watch   # 切换窗口，观察输出
```

**踩坑提示**：第 6 步务必确认 `WAYLAND_DISPLAY` 指向的是 Sway 的 socket（通常是 `wayland-1`）而不是 WSLg 的 `wayland-0`。若连到 Weston，会因缺少 `zwlr_foreign_toplevel_manager_v1` 而报错退出——这正是预期行为。

## 测试策略

CI（`.github/workflows/ci.yml`，四项 job 全部为硬性 gate）：

| 层 | 位置 | 验证内容 |
|---|---|---|
| build + clippy | `build` job | 编译 + `clippy --all-targets -D warnings` |
| 单测 + 契约 mock | `integration` job，`dbus-run-session cargo test` | 选择器三会话场景 / Dedup / Kwin.Report 契约（事件入通道、去重、空串→None）/ GNOME 轮询契约 |
| wlroots 端到端 | `integration` job | headless Sway + dummy-window：watch stdout 断言（**stdout/stderr 分流**，防日志假绿）、`busctl` 断言 GetFocus/FocusChanged、无 compositor 退出 |
| 冒烟 | `smoke` job | 无 Wayland 环境 probe 优雅退出 |

CI 无法运行真实 KDE/GNOME 桌面：真桌面行为由**契约 mock 测试 + 人工清单**
（`docs/MANUAL-TEST.md`）覆盖，不假装被自动化覆盖。

## 设计取舍

**为什么 `Focus` 只有两个字段？**
只承诺所有后端都能拿到的最小信息集。GNOME / KDE 后端将来未必能拿到 PID、几何等信息，现在承诺了就收不回来。

**为什么后端用阻塞 `run()` + channel？**
GNOME 需要跑 Shell 扩展、KDE 需要跑 KWin Script，它们的事件模型差异很大。用 channel 隔离后，上层（CLI / D-Bus）完全不需要知道底层是哪种机制。

**为什么去重放在主循环而不是各后端？**
所有事件源汇入同一条 channel；D-Bus 信号只应发射一次。去重收敛到一处（`Dedup`），后端内部去重只是优化，正确性由主循环保证。

## 路线图

- [x] **阶段一**：wlroots 后端 + CLI，验证核心假设（CI 无头环境已验证）
- [x] **阶段二**：D-Bus 接口 `org.focusd.Focus1`（`GetFocus()` + `FocusChanged`）+ 集成 gate 转正
- [x] **阶段三**：KDE KWin Script 后端（真机验证清单待执行）
- [x] **阶段四**：GNOME Shell Extension 后端（真机验证清单待执行）
- [ ] **阶段五**：对接 OpenLogi / Solaar，解决它们「按应用切换配置」在 Wayland 上的缺口

## 已知限制

- 协议版本固定绑定 3，未做版本协商（遇到只支持 v1/v2 的旧 compositor 会失败）
- KWin 会话重启后脚本不自动恢复，需重启 focusd（serve 常驻场景通常无感）
- 未处理 compositor 中途重启、display 断开重连
- 无权限模型：任何能连上同一 Wayland display 的程序都能用本后端（受 compositor 自身策略约束）

## 许可

MIT OR Apache-2.0
