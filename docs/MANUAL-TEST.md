# 真机人工验证清单

> CI 无法运行真实 KDE / GNOME 桌面，KWin Script 与 Shell Extension 的行为
> 只能用「zbus 起临时 mock 服务」做契约级验证（tests/kde_kwin_report.rs、
> tests/kde_kwin_health.rs、tests/gnome_poll.rs、tests/gnome_selfheal.rs）。
> 本清单是**人工步骤**，发布前在真机上逐项勾选；
> 任何一项失败不得标记该后端为"已验证"。

## 1. KDE Plasma（KWin Script 后端）

环境：真实 KDE Plasma 会话（Plasma 6 优先），Wayland 模式。

| # | 步骤 | 预期 | 结果 |
|---|---|---|---|
| K1 | `cargo build --release` 后 `./target/release/focusd probe` | `[✓] kde 可用`（wlroots 不可用） | ☐ |
| K2 | `./target/release/focusd watch --backend kde` | 日志出现「KWin 脚本已加载」 | ☐ |
| K3 | 切换到 Firefox / 终端等不同应用 | stdout 输出对应 `resourceClass` + 标题 | ☐ |
| K4 | 同一窗口内改标题（如终端里 cd 换目录） | 输出更新为新标题（captionChanged 链路） | ☐ |
| K5 | 切到桌面空白处（无窗口焦点） | 输出两列均为 `-`（空串→None） | ☐ |
| K6 | `./target/release/focusd serve --backend kde` 后<br>`busctl --user call org.focusd.Focus1 /org/focusd/Focus1 org.focusd.Focus1 GetFocus` | 返回当前焦点 resourceClass/标题 | ☐ |
| K7 | serve 运行中切窗，`gdbus monitor --session --dest org.focusd.Focus1` | 收到 `FocusChanged`，与实际切换一致 | ☐ |
| K8 | `busctl --user call org.focusd.Focus1 /org/focusd/Focus1 org.focusd.Focus1.Kwin Report test.app "测试"`（serve 运行中） | serve 日志出现去重后的焦点变化 | ☐ |
| K9 | serve **保持运行**，重启 KWin（`qdbus org.kde.KWin /KWin reconfigure` 或 `kwin_wayland --replace`；**不要**重启 focusd） | ≤ `FOCUSD_KWIN_HEALTH_MS`（默认 10s）内自动重新注册脚本并恢复上报：日志出现 `RUST_LOG=info` 下的「KWin 脚本已自动重新注册…**无需重启 focusd**」与「KWin 脚本已恢复在线」；**降级期间** `GetFocus` 返回 `ss "" ""` 且收到一次 `FocusChanged("","")`；随后补推当前焦点（不等用户切窗） | ☐ |
| K9b | 同上，但先让自动重注册**必然失败**（如在 系统设置 → 窗口管理 → KWin 脚本 里禁用 focusd，或临时破坏 `$XDG_DATA_HOME/focusd/kwin/main.js`） | 持续上报无焦点（`GetFocus` → `ss "" ""`）；`RUST_LOG=debug` 下出现 WARN，文案含「系统设置 → 窗口管理 → KWin 脚本」与「`kpackagetool6 --type=KWin/Script -i` 重装」和「**无需重启 focusd**」；排除故障后**无需重启 focusd** 即自动恢复 | ☐ |
| K10 | 手动安装路径：`kpackagetool6 --type=KWin/Script -i packaging/kde/org.focusd.kwin`，在 系统设置→窗口管理→KWin 脚本 启用 focusd | 脚本启用，K3/K7 通过 | ☐ |
| K11 | 阻断 D-Bus（如临时改名脚本里的 SERVICE）再 `focusd watch --backend kde` | 给出含 kpackagetool6 指引的可操作错误而非挂死 | ☐ |

## 2. GNOME（Shell Extension 后端）

环境：真实 GNOME 会话（Shell 45+，Wayland）。

| # | 步骤 | 预期 | 结果 |
|---|---|---|---|
| G1 | `cp -r packaging/gnome/focusd@rayc2026.github.io ~/.local/share/gnome-shell/extensions/`<br>注销重登（Wayland 下无法热重载 Shell） | 扩展出现在扩展列表 | ☐ |
| G2 | `gnome-extensions enable focusd@rayc2026.github.io`<br>`busctl --user list | grep focusd` | 总线上出现 `org.focusd.Gnome1` | ☐ |
| G3 | `./target/release/focusd probe` | `[✓] gnome 可用` | ☐ |
| G4 | `./target/release/focusd watch --backend gnome` | 窗口切换后输出对应 WM_CLASS + 标题 | ☐ |
| G5 | 切到桌面（无焦点窗口） | 输出两列均为 `-` | ☐ |
| G6 | `./target/release/focusd serve --backend gnome` + busctl 调 GetFocus / monitor FocusChanged | GetFocus 返回当前 WM_CLASS；切窗后收到 FocusChanged | ☐ |
| G7 | 用 `RUST_LOG=debug ./target/release/focusd serve --backend gnome` 启动（**必须显式给 `RUST_LOG`**：`env_logger` 默认级别是 `error`，不写连 `info` 都看不到），然后在 serve 运行中 `gnome-extensions disable focusd@rayc2026.github.io` | serve 不崩溃；**`GetFocus` 返回 `ss "" ""`**（不是上一次的陈旧值——这是本迭代根治的「常驻但说谎」），且 `gdbus monitor --session --dest org.focusd.Focus1` 收到**恰好一次** `FocusChanged("","")`；stderr 出现含 `gnome-extensions enable focusd@rayc2026.github.io` 的 WARN | ☐ |
| G7b | 接 G7，`gnome-extensions enable focusd@rayc2026.github.io`（**不要**重启 focusd） | ≤1 个轮询周期（`FOCUSD_POLL_MS`，默认 250ms；连续失败达 `FOCUSD_GNOME_FAIL_AFTER` 默认 3 次会重建 Proxy）后恢复上报真实 WM_CLASS，`GetFocus` 回到真实值，并出现 INFO 日志「Shell 扩展已恢复（此前连续失败 N 次…），无需重启 focusd」——**没有这条 INFO 就说明没真正恢复** | ☐ |
| G8 | Alt-Tab 快速连续切换 | GetFocus/输出最终停留在最后焦点窗口，无错乱 | ☐ |
| G9 | Wayland（非 X11）会话下完成 G4 | 确认扩展路径在 Wayland 下工作 | ☐ |

## 3. 回归（wlroots 系，任何有 Sway/Hyprland 的机器）

| # | 步骤 | 预期 | 结果 |
|---|---|---|---|
| W1 | `focusd watch` 切窗 | 输出 app_id + title（与 CI 断言一致） | ☐ |
| W2 | `focusd serve` + busctl GetFocus / monitor | 与 CI 集成断言一致 | ☐ |

## 4. systemd user unit

| # | 步骤 | 预期 | 结果 |
|---|---|---|---|
| S1 | `cp packaging/systemd/focusd.service ~/.config/systemd/user/`<br>`systemctl --user daemon-reload`<br>`systemctl --user enable --now focusd` | 服务 active，`journalctl --user -u focusd` 有「D-Bus 服务就绪」 | ☐ |
| S2 | `busctl --user call org.focusd.Focus1 /org/focusd/Focus1 org.focusd.Focus1 GetFocus` | 正常返回 | ☐ |
| S3 | `systemctl --user stop focusd` | 总线上 bus name 消失 | ☐ |
