# focusd 对接指南（Integration Guide）

> 面向想在自家工具里消费 focusd 焦点数据的开发者（Solaar / logiops / OpenLogi
> 或任何 Wayland 下的程序）。接口契约的权威定义见 [dbus.md](dbus.md)，
> 本文只讲**怎么接、有哪些坑**。

## 总览

```
                 D-Bus session bus
  focusd serve ──┬── org.focusd.Focus1 /org/focusd/Focus1
   (wlroots/KDE/ │      ├─ method  GetFocus() → (ss app_id, ss title)
    GNOME 后端)  │      └─ signal  FocusChanged(ss app_id, ss title)
                 └── org.focusd.Focus1.Kwin.Report   （KDE 推送入口，一般不用管）
```

消费方只需要关心 `org.focusd.Focus1` 这一个接口，两条规则：

1. **先查后听**：启动时调一次 `GetFocus()` 拿初始快照，再订阅
   `FocusChanged` 接增量。信号只在变化时发射、**不会补发当前值**——
   只听不查会错过 focusd 启动前就已存在的焦点。
2. **空串 = 无焦点**：锁屏、桌面空白处、所有窗口关闭时，`app_id` 与
   `title` 都是空串 `""`。请映射回你语言里的 `None/null`，不要把它当
   成一个名叫 `""` 的应用去匹配配置。

## 示例一：Python / GLib（Solaar 同栈，推荐）

```python
#!/usr/bin/env python3
"""focusd 消费示例：先查后听，维护当前焦点快照。"""
import gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

BUS, PATH, IFACE = "org.focusd.Focus1", "/org/focusd/Focus1", "org.focusd.Focus1"
state = {"app_id": None, "title": None}


def apply(app_id: str, title: str):
    """D-Bus 空串 → None 的语义映射在这里集中做一次。"""
    state["app_id"] = app_id or None
    state["title"] = title or None
    print(f"[focus] app_id={state['app_id']!r} title={state['title']!r}")


def on_changed(conn, sender, path, iface, signal, params):
    apply(*params.unpack())          # (ss) → (app_id, title)


session = Gio.bus_get_sync(Gio.BusType.SESSION, None)

# 1) 先查：初始快照
init = session.call_sync(BUS, PATH, IFACE, "GetFocus", None,
                         GLib.VariantType("(ss)"),
                         Gio.DBusCallFlags.NONE, -1, None).unpack()
apply(*init)

# 2) 后听：增量更新
session.signal_subscribe(BUS, IFACE, "FocusChanged", PATH, None,
                         Gio.DBusSignalFlags.NONE, on_changed)

GLib.MainLoop().run()
```

Solaar 接入建议：把 `apply()` 里的逻辑换成「按 `app_id` 查规则表 →
调用设备切换」，`signal_subscribe` 挂到其现有的 GLib 主循环上即可，
无需新线程。

## 示例二：Rust / zbus（OpenLogi 同栈）

```rust
// Cargo.toml: zbus = "5"（async API；消费方自行选择 runtime，
// 下例用 zbus 自带的 futures_lite 即可，无 tokio 依赖）
use futures_util::StreamExt;
use zbus::{Connection, Proxy};

const BUS: &str = "org.focusd.Focus1";
const PATH: &str = "/org/focusd/Focus1";
const IFACE: &str = "org.focusd.Focus1";

fn main() -> zbus::Result<()> {
    futures_lite::future::block_on(async {
        let conn = Connection::session().await?;
        let proxy = Proxy::new(&conn, BUS, PATH, IFACE).await?;

        // 先查
        let (app_id, title): (String, String) = proxy.call("GetFocus", &()).await?;
        println!("initial: {app_id:?} {title:?}");

        // 后听
        let mut signals = proxy.receive_signal("FocusChanged").await?;
        while let Some(msg) = signals.next().await {
            let (app_id, title): (String, String) = msg.body().deserialize()?;
            println!("[focus] {app_id:?} {title:?}");
        }
        Ok(())
    })
}
```

OpenLogi 接入建议：它已经有 per-application profile overlay 与 TOML 配置
（`app_id` 匹配键语义一致）。最小接法是在其 agent 线程里跑上面这段，
`app_id` 变化时触发 profile 切换——与它现有的 `sway`/`hyprland` 窗口
监听是平行数据源，加一个 feature flag 即可共存。

## 示例三：Shell（零依赖，调试/轻量自动化）

```bash
#!/bin/sh
# 先查
busctl --user call org.focusd.Focus1 /org/focusd/Focus1 \
  org.focusd.Focus1 GetFocus
# 后听（每行一条 FocusChanged）
gdbus monitor --session --dest org.focusd.Focus1 \
  --object-path /org/focusd/Focus1 | while read -r line; do
    case "$line" in *FocusChanged*) echo "$line";; esac
done
```

## 对接检查清单

- [ ] **session bus**：focusd 注册在用户会话总线上，连 `system bus` 会
      `ServiceUnknown`。
- [ ] **先查后听**：只订阅信号会错过启动前的既有焦点。
- [ ] **空串处理**：`""` → `None/null`，别参与配置匹配。
- [ ] **app_id 缺失兜底**：XWayland 窗口的 `app_id` 可能为空串或与
      desktop 文件名不一致（sway 会尝试映射 WM_CLASS）。匹配逻辑要有
      「app_id 为空 → 按 title 或按 last-resort 规则」的分支。
- [ ] **focusd 未启动容错**：捕获 `ServiceUnknown`（zbus: `zbus::Error::
      NameHasNoOwner`；GLib: `G_IO_ERROR_DBUS_ERROR`），按策略重试或
      降级为无焦点模式，不要让宿主程序因此崩溃。
- [ ] **不要高频轮询**：信号已驱动一切；GNOME 后端内部 250ms 的轮询是
      focusd 的实现细节，不是给消费方的建议。

## 真机验证（对接方自查）

`docs/MANUAL-TEST.md` 有 focusd 侧的完整清单。对接方最关心的三条：

1. `focusd serve` 运行时，切换/关闭窗口，D-Bus 信号实时到达；
2. 关闭最后一个窗口后 `GetFocus` 返回 `ss "" ""`；
3. 锁屏 → 解锁，信号序列无异常风暴。

## 已知边界

- KDE 后端依赖 KWin Script 被用户手动启用（`packaging/kde/` 有安装
  说明），且 **KWin 重启后脚本不自动恢复**，需重启 focusd；
- GNOME 后端需要用户安装 `packaging/gnome/focusd@rayc2026.github.io/`
  扩展；
- wlroots 后端（Sway/Hyprland/river/labwc 等）开箱即用，无额外组件。
