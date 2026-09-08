# focusd D-Bus 接口文档

> 接口签名的任何变更必须同步更新本文件（架构文档共享知识约定）。

## 总线名与对象

| 项 | 值 |
|---|---|
| Bus name | `org.focusd.Focus1` |
| 对象路径 | `/org/focusd/Focus1` |
| 接口 | `org.focusd.Focus1`（焦点查询/信号）、`org.focusd.Focus1.Kwin`（KWin 推送入口） |

提供方：`focusd serve`（D-Bus 服务模式）。
`focusd watch --backend kde` 也会注册 `org.focusd.Focus1.Kwin`（仅接收 KWin 推送，
不提供 GetFocus / FocusChanged —— 那是 serve 语义）。

## org.focusd.Focus1

```xml
<interface name="org.focusd.Focus1">
  <!-- 当前焦点快照；无焦点时两个字段均为空串 -->
  <method name="GetFocus">
    <arg name="app_id" type="s" direction="out"/>
    <arg name="title"  type="s" direction="out"/>
  </method>
  <!-- 仅在焦点真正变化（去重后）时发射 -->
  <signal name="FocusChanged">
    <arg name="app_id" type="s"/>
    <arg name="title"  type="s"/>
  </signal>
</interface>
```

语义约定：

- `Focus` 结构保持两字段（app_id / title），`None` 序列化为**空串 `""`**
  （D-Bus 没有 null 字符串）。消费方应以空串判断"无焦点"，不要期望 null。
- `app_id` 语义随所选后端而异：wlroots → Wayland `app_id`；
  KDE → KWin `resourceClass`；GNOME → **WM_CLASS**（X11 语义，与 Wayland
  app_id 不严格相等，多数应用两者一致）。后端语义差异见 README §测试策略。
- `FocusChanged` 只在快照真正变化（含从无到有）时发射；同值重发（如 GNOME
  轮询的 250ms 重复值）由主循环 Dedup 过滤，不会到达总线。
- "先写状态、后发信号"：信号订阅者收到 FocusChanged 后立即调 GetFocus
  一定能读到新值。

## org.focusd.Focus1.Kwin

```xml
<interface name="org.focusd.Focus1.Kwin">
  <!-- KWin 脚本 callDBus 推送入口；对 focusd 是 server 方法 -->
  <method name="Report">
    <arg name="app_id" type="s" direction="in"/>
    <arg name="title"  type="s" direction="in"/>
  </method>
</interface>
```

调用方是 KWin 脚本（packaging/kde/org.focusd.kwin），`app_id` 来自
`resourceClass`，`title` 来自 `caption`；无焦点窗口时发两个空串
（focusd 内部把空串还原为 `None`）。

## GNOME 扩展侧契约（focusd 作为 client）

| 项 | 值 |
|---|---|
| Bus name | `org.focusd.Gnome1` |
| 对象路径 | `/org/focusd/Gnome` |
| 接口 | `org.focusd.Gnome1` |

```xml
<interface name="org.focusd.Gnome1">
  <method name="GetFocus">
    <arg type="s" direction="out" name="wm_class"/>
    <arg type="s" direction="out" name="title"/>
  </method>
</interface>
```

提供方是 Shell 扩展（packaging/gnome/focusd@rayc2026.github.io）；focusd 按
`FOCUSD_POLL_MS`（默认 250ms，clamp 20ms–5s）轮询该方法。

## 使用示例

```bash
# 查看当前焦点
busctl --user call org.focusd.Focus1 /org/focusd/Focus1 org.focusd.Focus1 GetFocus
# 输出形如：  ss "firefox" "Mozilla Firefox"

# 订阅焦点变化
busctl --user monitor --match="interface=org.focusd.Focus1,member=FocusChanged"
# 或用 gdbus：
gdbus monitor --session --dest org.focusd.Focus1 --object-path /org/focusd/Focus1

# 确认接口在总线上可见
busctl --user introspect org.focusd.Focus1 /org/focusd/Focus1
```

## 消费方参考（Solaar / logiops / OpenLogi 等）

1. 启动时调一次 `GetFocus()` 获取初始状态；
2. 订阅 `FocusChanged` 增量更新（不要高频轮询）；
3. 空串 = 当前无焦点窗口（如锁屏 / 桌面空白处）；
4. `app_id` 用来匹配配置文件键（与 desktop 文件名 / WM_CLASS 对齐）。

## 未来迁移说明

`org.focusd.Focus1` 是简化的自有命名空间。若将来申请 fd.o namespaced
名称（如 `io.github.rayc2026.focusd`），只需：

1. `src/dbus/mod.rs` 中 `BUS_NAME` 常量改名（接口名/路径同步）；
2. 保留旧 bus name 一段时间做双注册过渡；
3. 消费方更新 dest 字符串即可（接口签名不变）。
