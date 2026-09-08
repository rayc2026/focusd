// focusd KWin script：把活动窗口推给 focusd 的 D-Bus 服务。
//
// KWin Scripting API 只能单向 callDBus 对外推送（无法注册 bus name、
// 无法导出方法、没有定时器），所以链路是「脚本订阅信号 → callDBus 推送」，
// focusd 侧接收后统一去重。
//
// 本文件与 focusd 二进制内嵌的脚本（src/backend/kde.rs include_str!）一致：
// 自动加载走内嵌副本，手动安装（kpackagetool6）走本文件。

const SERVICE = "org.focusd.Focus1";
const PATH    = "/org/focusd/Focus1";
const IFACE   = "org.focusd.Focus1.Kwin";

function report(w) {
    if (w) callDBus(SERVICE, PATH, IFACE, "Report",
                    w.resourceClass || "", w.caption || "");
}

// 1) 启动即报告当前活动窗口（补齐初始状态）
report(workspace.activeWindow);

// 2) 激活切换
workspace.windowActivated.connect(report);

// 3) 活动窗口标题变化（Plasma 6：window 对象信号）。
//    captionChanged 在各小版本签名略有差异，按「存在即连接」防御式处理。
workspace.windowAdded.connect(function (w) {
    if (w && w.captionChanged) {
        w.captionChanged.connect(function () {
            if (workspace.activeWindow === w) report(w);
        });
    }
});
