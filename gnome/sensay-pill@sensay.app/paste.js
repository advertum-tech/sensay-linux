// Pasting the dictated text into the focused window, done inside GNOME Shell.
//
// GNOME on Wayland lets no application set the clipboard or press keys for
// another window. The shell can do both, so the Sensay app hands the text to
// this extension over the session bus.
//
// When the focused field takes text from the input method (GTK, Qt 6.7 and
// later, Chromium and Electron on Wayland), the whole text goes in through it,
// as the on-screen keyboard does it: nothing on the clipboard, no keys, any
// layout. A terminal takes a line break from the input method as Enter, so
// text with one goes the clipboard way there, where Ctrl+Shift+V pastes it.
//
// Anywhere else, X11 windows among them, the extension does what
// ClipboardTextSink.swift does on macOS:
//
//     t0  the text goes on the clipboard
//     t1  Ctrl+V is pressed, Ctrl+Shift+V in a terminal     t1 - t0 = 12 ms
//     t2  the old clipboard goes back, if it is still ours  t2 - t1 = 500 ms
//
// Keys are sent as evdev codes, so they mean the same keys whatever layout is
// active: with Russian on top, Ctrl plus the V key still pastes.
//
// One difference from macOS: the old clipboard comes back in one format only,
// the one that matters most (files, then an image, then text). GNOME Shell
// can hold a clipboard of one format from an extension.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Clutter from 'gi://Clutter';
import Meta from 'gi://Meta';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const CLIPBOARD = Meta.SelectionType.SELECTION_CLIPBOARD;
const TEXT = 'text/plain;charset=utf-8';

// PasteTiming.swift.
const BEFORE_PASTE_MS = 12;
const BETWEEN_KEYS_MS = 8;
const BEFORE_RESTORE_MS = 500;
// The activation key is made of modifiers: wait for every modifier to be up
// before pressing Ctrl+V, as whisrs does.
const MODIFIER_POLL_MS = 15;
const MODIFIER_WAIT_MS = 3000;
const AFTER_MODIFIERS_MS = 25;

const KEY_LEFTCTRL = 29;
const KEY_LEFTSHIFT = 42;
const KEY_V = 47;

const HELD_MODIFIERS = Clutter.ModifierType.SHIFT_MASK |
    Clutter.ModifierType.CONTROL_MASK |
    Clutter.ModifierType.MOD1_MASK |
    Clutter.ModifierType.SUPER_MASK |
    Clutter.ModifierType.HYPER_MASK |
    Clutter.ModifierType.META_MASK;

// Window classes and app ids of terminals, where paste is Ctrl+Shift+V. The
// list is the one of whisrs (MIT), src/daemon/injection.rs.
const TERMINALS = new Set([
    'alacritty', 'blackbox-terminal', 'contour', 'cool-retro-term', 'deepin-terminal',
    'foot', 'footclient', 'ghostty', 'ghostty-debug', 'gnome-terminal',
    'gnome-terminal-server', 'guake', 'hyper', 'kgx', 'kitty', 'koi8rxterm', 'konsole',
    'lxterminal', 'mate-terminal', 'mlterm', 'ptyxis', 'qterminal', 'rio', 'roxterm',
    'rxvt', 'rxvt-unicode', 'sakura', 'st', 'st-16color', 'st-256color', 'st-direct',
    'st-mono', 'tabby', 'terminator', 'terminology', 'termite', 'tilix', 'urxvt',
    'urxvtc', 'uxterm', 'waveterm', 'wezterm', 'xfce4-terminal', 'xterm', 'yakuake',
    'com.gexperts.tilix', 'com.mitchellh.ghostty', 'com.mitchellh.ghostty-debug',
    'com.raggesilver.blackbox', 'dev.warp.warp', 'io.elementary.terminal',
    'org.contourterminal.contour', 'org.gnome.console', 'org.gnome.console.devel',
    'org.gnome.ptyxis', 'org.gnome.terminal', 'org.kde.konsole', 'org.kde.yakuake',
    'org.wezfurlong.wezterm', 'org.xfce.terminal',
]);
const TERMINAL_LEAVES = new Set([
    'alacritty', 'cool-retro-term', 'ghostty', 'ghostty-debug', 'guake', 'kitty',
    'konsole', 'ptyxis', 'qterminal', 'tilix', 'waveterm', 'wezterm', 'yakuake',
]);

function isTerminal(windowClass) {
    const name = (windowClass ?? '').trim().toLowerCase();
    if (!name)
        return false;
    if (TERMINALS.has(name))
        return true;
    const dot = name.lastIndexOf('.');
    return dot >= 0 && TERMINAL_LEAVES.has(name.slice(dot + 1));
}

// The one format the old clipboard is kept in: files first, so that a copy
// in the file manager still pastes as files; then an image; then text.
function pickFormat(formats) {
    const order = ['x-special/gnome-copied-files', 'text/uri-list', 'image/png'];
    for (const format of order) {
        if (formats.includes(format))
            return format;
    }
    const image = formats.find(format => format.startsWith('image/'));
    if (image)
        return image;
    for (const format of [TEXT, 'UTF8_STRING', 'text/plain', 'STRING']) {
        if (formats.includes(format))
            return format;
    }
    return formats[0] ?? null;
}

export class Paster {
    constructor() {
        this._selection = global.display.get_selection();
        const seat = global.stage.context.get_backend().get_default_seat();
        this._keyboard = seat.create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
        this._timeouts = new Set();
        // One paste at a time: each holds the clipboard for half a second.
        this._tail = Promise.resolve();
        this._destroyed = false;
    }

    destroy() {
        this._destroyed = true;
        for (const id of this._timeouts)
            GLib.source_remove(id);
        this._timeouts.clear();
        this._keyboard = null;
    }

    // Resolves with a report for the app's log.
    paste(text) {
        const run = this._tail.then(() => this._paste(text));
        this._tail = run.catch(() => {});
        return run;
    }

    _sleep(ms) {
        return new Promise(resolve => {
            const id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
                this._timeouts.delete(id);
                resolve();
                return GLib.SOURCE_REMOVE;
            });
            this._timeouts.add(id);
        });
    }

    async _paste(text) {
        const started = GLib.get_monotonic_time();
        const report = {};

        const focused = global.display.get_focus_window();
        const terminal = isTerminal(focused?.get_wm_class());
        if (Main.inputMethod.currentFocus && !(terminal && text.includes('\n'))) {
            report.path = 'input-method';
            report.target = focused?.get_wm_class() ?? focused?.get_title() ?? '(no window)';
            Main.inputMethod.commit(text);
            report.totalMs = Math.round((GLib.get_monotonic_time() - started) / 1000);
            return report;
        }
        report.path = 'clipboard';

        const saved = await this._snapshot();
        report.before = saved
            ? `${saved.format}, ${saved.bytes.get_size()} bytes`
            : '(empty)';

        // Ours from here until somebody else takes the clipboard.
        const mine = Meta.SelectionSourceMemory.new(TEXT, new GLib.Bytes(new TextEncoder().encode(text)));
        let ours = true;
        const watch = this._selection.connect('owner-changed', (_selection, type, source) => {
            if (type === CLIPBOARD && source !== mine)
                ours = false;
        });
        this._selection.set_owner(CLIPBOARD, mine);

        try {
            await this._sleep(BEFORE_PASTE_MS);
            const held = await this._waitForModifiers();
            report.waitedMs = held.waited;
            if (held.stillHeld) {
                // Ctrl+V on top of held keys would be some other shortcut. The
                // text stays on the clipboard for a paste by hand.
                report.keys = 'none, modifiers still held';
                report.restore = 'text left on the clipboard';
                return report;
            }

            const target = global.display.get_focus_window();
            report.target = target?.get_wm_class() ?? target?.get_title() ?? '(no window)';
            const terminal = isTerminal(target?.get_wm_class());
            report.keys = terminal ? 'Ctrl+Shift+V' : 'Ctrl+V';
            await this._pressPaste(terminal);

            await this._sleep(BEFORE_RESTORE_MS);
            if (!ours) {
                // Somebody replaced the clipboard between the keystroke and now.
                report.restore = 'clipboard changed meanwhile, left as it is';
            } else if (saved) {
                this._selection.set_owner(CLIPBOARD, Meta.SelectionSourceMemory.new(saved.format, saved.bytes));
                report.restore = `restored ${saved.format}`;
            } else {
                this._selection.unset_owner(CLIPBOARD, mine);
                report.restore = 'cleared, it was empty';
            }
            return report;
        } finally {
            this._selection.disconnect(watch);
            report.totalMs = Math.round((GLib.get_monotonic_time() - started) / 1000);
        }
    }

    async _snapshot() {
        const formats = this._selection.get_mimetypes(CLIPBOARD);
        const format = pickFormat(formats);
        if (!format)
            return null;
        try {
            const stream = Gio.MemoryOutputStream.new_resizable();
            await new Promise((resolve, reject) => {
                this._selection.transfer_async(CLIPBOARD, format, -1, stream, null, (selection, result) => {
                    try {
                        selection.transfer_finish(result);
                        resolve();
                    } catch (error) {
                        reject(error);
                    }
                });
            });
            stream.close(null);
            return {format, bytes: stream.steal_as_bytes()};
        } catch (error) {
            console.warn(`Sensay pill: could not read the clipboard as ${format}: ${error.message}`);
            return null;
        }
    }

    async _waitForModifiers() {
        let waited = 0;
        for (;;) {
            const [, , mods] = global.get_pointer();
            if ((mods & HELD_MODIFIERS) === 0)
                break;
            if (waited >= MODIFIER_WAIT_MS)
                return {waited, stillHeld: true};
            await this._sleep(MODIFIER_POLL_MS);
            waited += MODIFIER_POLL_MS;
        }
        if (waited > 0)
            await this._sleep(AFTER_MODIFIERS_MS);
        return {waited, stillHeld: false};
    }

    // Every event, the modifiers included: apps that track modifiers
    // themselves need to see them go down and up.
    async _pressPaste(terminal) {
        const keys = terminal ? [KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V] : [KEY_LEFTCTRL, KEY_V];
        for (const key of keys) {
            this._key(key, Clutter.KeyState.PRESSED);
            await this._sleep(BETWEEN_KEYS_MS);
        }
        for (const key of keys.reverse()) {
            this._key(key, Clutter.KeyState.RELEASED);
            if (key !== KEY_LEFTCTRL)
                await this._sleep(BETWEEN_KEYS_MS);
        }
    }

    _key(code, state) {
        if (this._destroyed || !this._keyboard)
            return;
        this._keyboard.notify_key(GLib.get_monotonic_time(), code, state);
    }
}
