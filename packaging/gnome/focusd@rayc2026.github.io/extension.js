// focusd GNOME Shell 扩展：把焦点窗口（WM_CLASS + title）经 D-Bus 暴露。
//
// 契约：own `org.focusd.Gnome1`，导出 `GetFocus() -> (s wm_class, s title)`，
// 对象路径 /org/focusd/Gnome。focusd（GnomeBackend）按 FOCUSD_POLL_MS 轮询。
//
// 说明：GNOME 拿到的是 WM_CLASS（X11 语义），与 wlroots 的 app_id
// 不严格相等——大多数应用两者一致。无焦点窗口时返回两个空串。

import Gio from 'gi://Gio';

const IFACE_XML = `
<node>
  <interface name="org.focusd.Gnome1">
    <method name="GetFocus">
      <arg type="s" direction="out" name="wm_class"/>
      <arg type="s" direction="out" name="title"/>
    </method>
  </interface>
</node>`;

export default class FocusdExtension {
    enable() {
        this._iface = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        // own_name 返回 owner id（用于 disable 时 unown）
        this._owner_id = Gio.DBus.session.own_name(
            'org.focusd.Gnome1',
            Gio.BusNameOwnerFlags.NONE,
            null,
            null,
        );
        this._iface.export(Gio.DBus.session, '/org/focusd/Gnome');
    }

    // 由 GJS 按 IFACE_XML 分派；返回 [wm_class, title]
    GetFocus() {
        const w = global.display.focus_window;
        return w ? [w.get_wm_class() ?? '', w.get_title() ?? ''] : ['', ''];
    }

    disable() {
        if (this._iface) {
            this._iface.unexport();
            this._iface = null;
        }
        if (this._owner_id !== undefined) {
            Gio.DBus.session.unown_name(this._owner_id);
            this._owner_id = undefined;
        }
    }
}
