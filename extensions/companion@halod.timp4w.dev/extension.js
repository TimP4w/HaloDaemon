import Gio from "gi://Gio";
import GLib from "gi://GLib";
import { Extension } from "resource:///org/gnome/shell/extensions/extension.js";

const OBJECT_PATH = "/dev/timp4w/halod/FocusWatcher1";
const INTERFACE_XML = `
<node>
  <interface name="dev.timp4w.halod.FocusWatcher1">
    <method name="GetFocused">
      <arg type="s" direction="out" name="name"/>
    </method>
    <signal name="FocusChanged">
      <arg type="s" name="name"/>
    </signal>
  </interface>
</node>`;

const WINE_BINS = ["wine", "wine64", "wine-preloader", "wine64-preloader"];

export default class HalodFocusWatcher extends Extension {
  enable() {
    this._lastName = null;
    this._dbus = Gio.DBusExportedObject.wrapJSObject(INTERFACE_XML, this);
    this._dbus.export(Gio.DBus.session, OBJECT_PATH);
    this._focusConn = global.display.connect("notify::focus-window", () => {
      this._onFocusChanged();
    });
    this._onFocusChanged();
  }

  disable() {
    if (this._focusConn) {
      global.display.disconnect(this._focusConn);
      this._focusConn = null;
    }
    if (this._dbus) {
      this._dbus.unexport();
      this._dbus = null;
    }
    this._lastName = null;
  }

  GetFocused() {
    return this._focusedName();
  }

  _onFocusChanged() {
    const name = this._focusedName();
    if (name === this._lastName) return;
    this._lastName = name;
    this._dbus.emit_signal("FocusChanged", new GLib.Variant("(s)", [name]));
  }

  _focusedName() {
    const pid = global.display.focus_window?.get_pid();
    if (!pid || pid <= 0) return "";
    try {
      const exe = GLib.file_read_link(`/proc/${pid}/exe`);
      const name = GLib.path_get_basename(exe).toLowerCase();
      if (WINE_BINS.includes(name)) return this._wineExeName(pid) ?? name;
      return name;
    } catch (_e) {
      return "";
    }
  }

  _wineExeName(pid) {
    try {
      const [, bytes] = GLib.file_get_contents(`/proc/${pid}/cmdline`);
      const args = new TextDecoder().decode(bytes).split("\0");
      const basename = (p) => p.split(/[\\/]/).pop().toLowerCase();
      const isSystem = (a) => a.toLowerCase().includes("\\windows\\");

      const exeArg = args.find(
        (a) => a.toLowerCase().endsWith(".exe") && !isSystem(a),
      );
      if (exeArg) return basename(exeArg).replace(/\.exe$/, "") || null;

      const winPath = args.find((a) => /^[a-z]:\\/i.test(a) && !isSystem(a));
      if (winPath) return basename(winPath) || null;

      return null;
    } catch (_e) {
      return null;
    }
  }
}
