import Gio from "gi://Gio";
import GLib from "gi://GLib";
import { Extension } from "resource:///org/gnome/shell/extensions/extension.js";

const WINE_BINS = ["wine", "wine64", "wine-preloader", "wine64-preloader"];

export default class HalodFocusWatcher extends Extension {
  enable() {
    this._focusConn = global.display.connect("notify::focus-window", () => {
      this._onFocusChanged();
    });
    // Emit for the window that's already focused when we enable.
    this._onFocusChanged();
  }

  disable() {
    if (this._focusConn) {
      global.display.disconnect(this._focusConn);
      this._focusConn = null;
    }
  }

  _onFocusChanged() {
    const win = global.display.focus_window;
    if (!win) return;
    const pid = win.get_pid();
    if (!pid) return;
    try {
      const exe = GLib.file_read_link(`/proc/${pid}/exe`);
      let name = GLib.path_get_basename(exe).toLowerCase();

      // Wine/Proton: the exe is the Wine loader, not the game.
      // Extract the actual Windows exe stem from the process cmdline.
      if (WINE_BINS.includes(name)) {
        const winName = this._wineExeName(pid);
        if (winName) name = winName;
      }

      if (!name) return;
      Gio.DBus.session.emit_signal(
        null,
        "/dev/timp4w/halod/FocusWatcher1",
        "dev.timp4w.halod.FocusWatcher1",
        "FocusChanged",
        new GLib.Variant("(s)", [name]),
      );
    } catch (_e) {
      // Kernel threads or processes whose exe link has gone — ignore.
    }
  }

  _wineExeName(pid) {
    try {
      const [, bytes] = GLib.file_get_contents(`/proc/${pid}/cmdline`);
      const args = new TextDecoder().decode(bytes).split("\0");

      // Prefer an arg ending in .exe.
      const exeArg = args.find((a) => a.toLowerCase().endsWith(".exe"));
      if (exeArg) {
        const base = GLib.path_get_basename(exeArg);
        return base.replace(/\.exe$/i, "").toLowerCase() || null;
      }

      // Fallback: Wine maps the Linux fs under z:\ — match any Windows-style
      // path (e.g. "z:\...\ds") and take its basename.
      const winPath = args.find((a) => /^[a-z]:\\/i.test(a));
      if (winPath) {
        const parts = winPath.split("\\");
        const base = parts[parts.length - 1].toLowerCase();
        return base || null;
      }

      return null;
    } catch (_e) {
      return null;
    }
  }
}
