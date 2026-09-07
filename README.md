# focusd

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

## 当前状态：MVP

**只实现了 wlroots 一条路径。** 这是刻意的——先用最小代码验证核心假设：

> 能否稳定、及时、低开销地拿到焦点 app_id？

如果这个假设不成立，后面的 D-Bus 接口、多后端适配全是白做。

- ✅ 连接 wlroots 系 compositor，实时输出焦点变化的 `app_id` + `title`
- ✅ 去重（只在焦点真正变化时推送）
- ❌ D-Bus 接口（第二阶段）
- ❌ GNOME / KDE 后端（第二阶段）

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

运行要求：**必须在 Wayland 会话中，且 compositor 属于 wlroots 系**（Sway、Hyprland、river、labwc、Wayfire 等）。GNOME / KDE 会在运行时报错退出——这是当前预期行为，不是 bug。

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

## 构建与运行

```bash
cargo build --release

# 先探测环境
./target/release/focusd probe

# 监听焦点变化（切换窗口观察输出）
./target/release/focusd watch

# JSON 输出，便于被其它程序消费
./target/release/focusd watch --format json
```

预期输出（切到 Firefox 再切到终端）：
```
firefox	Mozilla Firefox
foot	~
```

## 设计取舍

**为什么先不做 D-Bus？**
D-Bus 是给外部程序用的接口。在确认「能稳定拿到 app_id」之前加 D-Bus，等于同时调试两件事。先让 CLI 跑通。

**为什么 `Focus` 只有两个字段？**
只承诺所有后端都能拿到的最小信息集。GNOME / KDE 后端将来未必能拿到 PID、几何等信息，现在承诺了就收不回来。

**为什么后端用阻塞 `run()` + channel？**
GNOME 需要跑 Shell 扩展、KDE 需要跑 KWin Script，它们的事件模型差异很大。用 channel 隔离后，上层（CLI / D-Bus）完全不需要知道底层是哪种机制。

## 路线图

- [ ] **阶段一（当前）**：wlroots 后端 + CLI，验证核心假设
- [ ] **阶段二**：D-Bus 接口 `org.focusd.Focus1`（`GetFocus()` 方法 + `FocusChanged` 信号）
- [ ] **阶段三**：KDE KWin Script 后端
- [ ] **阶段四**：GNOME Shell Extension 后端
- [ ] **阶段五**：对接 OpenLogi / Solaar，解决它们「按应用切换配置」在 Wayland 上的缺口

## 已知限制

- 协议版本固定绑定 3，未做版本协商（遇到只支持 v1/v2 的旧 compositor 会失败）
- `state` 事件按 native endian 解析 `array<uint32>`，未处理跨字节序场景（实际不影响 x86/arm 主流平台）
- 未处理 compositor 中途重启、display 断开重连
- 无权限模型：任何能连上同一 Wayland display 的程序都能用本后端（受 compositor 自身策略约束）

## 许可

MIT OR Apache-2.0
