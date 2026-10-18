// Sensay pill for GNOME Shell.
//
// GNOME on Wayland gives an application no way to keep a window above all
// others without taking focus, so the Sensay app does not draw its pill
// here: this extension does, from what the app sends on the session bus.
//
//   object    /com/advertumtech/Sensay/Pill
//   interface com.advertumtech.Sensay.Pill
//   signal    StateChanged(s)   "idle", "listening" or "waiting"
//   signal    Level(d)          0...1, 60 times a second while listening
//   method    GetState() -> s
//
// It also pastes the dictated text for the app (paste.js), serving
//
//   object    /com/advertumtech/Sensay/Shell, on GNOME Shell's connection
//   interface com.advertumtech.Sensay.Shell
//   method    Paste(s text) -> s report, JSON for the app's log
//
// to the current owner of com.advertumtech.Sensay only.
//
// The pill is on screen while the app owns com.advertumtech.Sensay and gone
// when it does not. Its look and motion follow PillView.swift and Theme.swift
// of the macOS version, with one change: the open pill is lighter, so the
// window under it shows through while recording too.
//
// Everything is drawn with cairo on one St.DrawingArea that never changes
// size: it is wide enough for the waiting pill, and the capsule inside is
// what changes. Nothing in it is reactive, so clicks reach the windows under
// it and the focus never moves.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Clutter from 'gi://Clutter';
import St from 'gi://St';
import Cairo from 'cairo';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

import {Paster} from './paste.js';

const BUS_NAME = 'com.advertumtech.Sensay';
const PILL_PATH = '/com/advertumtech/Sensay/Pill';
const PILL_INTERFACE = 'com.advertumtech.Sensay.Pill';
const STATES = ['idle', 'listening', 'waiting'];

const SHELL_PATH = '/com/advertumtech/Sensay/Shell';
const SHELL_INTERFACE = `
<node>
  <interface name="com.advertumtech.Sensay.Shell">
    <method name="Paste">
      <arg type="s" direction="in" name="text"/>
      <arg type="s" direction="out" name="report"/>
    </method>
  </interface>
</node>`;

// Geometry, in logical pixels. Every measurement comes from the height.
const HEIGHT = 32;
const EXPANDED = {w: HEIGHT * 2.47, h: HEIGHT};
const IDLE = {w: HEIGHT * 1.36, h: HEIGHT * 0.29};
const BOTTOM_INSET = 24;

const BAR_COUNT = 10;
const BAR_WIDTH = HEIGHT * 0.066;
const BAR_GAP = HEIGHT * 0.070;
const BAR_PITCH = BAR_WIDTH + BAR_GAP;
const METER_WIDTH = BAR_COUNT * BAR_WIDTH + (BAR_COUNT - 1) * BAR_GAP;
const BAR_MIN = HEIGHT * 0.035;
const BAR_MAX = HEIGHT * 0.47;
const BAR_REST = HEIGHT * 0.074;
const BAR_LOUD = HEIGHT * 0.39;
const BELL_WIDTH = 6.0;
const WAVE_PERIOD = 1.0;
const WAVE_FIRST_AMPLITUDE = 0.14;
const WAVE_FIRST_PHASE = -3.03;
const WAVE_SECOND_AMPLITUDE = 0.073;
const WAVE_SECOND_PHASE = 0.60;
// Grey dots stand in for the bars until this long after the expansion.
const DOTS_DURATION = 0.09;
// Each bar's brightness wanders in this range, one eased move per step.
const SHIMMER_RANGE = [0.82, 1.0];
const SHIMMER_STEP = [0.4, 1.5];
// Smoothing of the level, rising and falling, in seconds.
const LEVEL_ATTACK = 0.040;
const LEVEL_RELEASE = 0.080;

const SPINNER_RAYS = 8;
const SPINNER_SCALE = 2 / 3;
const SPINNER_INNER = HEIGHT * 0.117 * SPINNER_SCALE;
const SPINNER_OUTER = HEIGHT * 0.280 * SPINNER_SCALE;
const SPINNER_THICKNESS = BAR_WIDTH * SPINNER_SCALE;
const SPINNER_TO_END = HEIGHT / 2;
const SPINNER_OFFSET = METER_WIDTH / 2 + HEIGHT / 2;
const SPINNER_STEP = 8 / 60;
const SPINNER_FADE_PER_STEP = 0.104;
const SPINNER_FIRST_RAY = 7;

const WAITING_GROWTH = SPINNER_OFFSET + SPINNER_TO_END - EXPANDED.w / 2;
const WAITING = {w: EXPANDED.w + WAITING_GROWTH, h: HEIGHT};
const CANVAS = {w: EXPANDED.w + 2 * WAITING_GROWTH, h: HEIGHT};

// Fill is black, border and bars white. The macOS open pill is 0.85.
const FILL_IDLE = 0.5;
const FILL_ACTIVE = 0.6;
const BORDER_IDLE = 0.42;
const BORDER_ACTIVE = 0.08;
const DOT_ALPHA = 0.32;

// Cubic Bezier control points x1, y1, x2, y2.
const EXPAND = {duration: 0.10, curve: [0, 0.5, 0, 1]};
const COLLAPSE = {duration: 0.12, curve: [0.2, 0.85, 0.4, 0.95]};

const now = () => GLib.get_monotonic_time() / 1e6;
const clamp = (value, low, high) => Math.min(high, Math.max(low, value));
const randomIn = ([low, high]) => low + Math.random() * (high - low);
// Fraction of the way to a target covered in dt with time constant tau.
const step = (dt, tau) => 1 - Math.exp(-dt / tau);

// The CSS cubic-bezier timing function: progress for time u in 0...1.
function bezier([x1, y1, x2, y2], u) {
    if (u <= 0)
        return 0;
    if (u >= 1)
        return 1;
    const cx = 3 * x1, bx = 3 * (x2 - x1) - cx, ax = 1 - cx - bx;
    const cy = 3 * y1, by = 3 * (y2 - y1) - cy, ay = 1 - cy - by;
    const curveX = t => ((ax * t + bx) * t + cx) * t;
    const curveY = t => ((ay * t + by) * t + cy) * t;
    let low = 0, high = 1, t = u;
    for (let i = 0; i < 40; i++) {
        const x = curveX(t);
        if (Math.abs(x - u) < 1e-6)
            break;
        if (x < u)
            low = t;
        else
            high = t;
        t = (low + high) / 2;
    }
    return curveY(t);
}

// Constant per-bar factor: 1.0 in the middle, 0.57 at the outer bars.
function bell(index) {
    const distance = (index - (BAR_COUNT - 1) / 2) / BELL_WIDTH;
    return Math.exp(-distance * distance);
}

// The ripple as a fraction of the bar's own height, travelling left to right
// along the whole row once per period.
function wave(time, index) {
    const phase = time / WAVE_PERIOD - index / BAR_COUNT;
    return WAVE_FIRST_AMPLITUDE * Math.cos(2 * Math.PI * phase + WAVE_FIRST_PHASE) +
        WAVE_SECOND_AMPLITUDE * Math.cos(4 * Math.PI * phase + WAVE_SECOND_PHASE);
}

function barTarget(index, level, time) {
    const middle = BAR_REST + (BAR_LOUD - BAR_REST) * level;
    return clamp(middle * bell(index) * (1 + wave(time, index)), BAR_MIN, BAR_MAX);
}

function roundedRect(cr, x, y, w, h, radius) {
    const r = Math.max(0, Math.min(radius, w / 2, h / 2));
    cr.newSubPath();
    cr.arc(x + w - r, y + r, r, -Math.PI / 2, 0);
    cr.arc(x + w - r, y + h - r, r, 0, Math.PI / 2);
    cr.arc(x + r, y + h - r, r, Math.PI / 2, Math.PI);
    cr.arc(x + r, y + r, r, Math.PI, 3 * Math.PI / 2);
    cr.closePath();
}

// Where the capsule sits in the canvas for a state. The bottom edge never
// moves, and neither does the centre, except while waiting: then the left
// edge stays where the listening pill had it and the pill grows right.
function capsuleFor(state) {
    const size = state === 'idle' ? IDLE : state === 'listening' ? EXPANDED : WAITING;
    const left = state === 'waiting'
        ? (CANVAS.w - EXPANDED.w) / 2
        : (CANVAS.w - size.w) / 2;
    return {
        left,
        w: size.w,
        h: size.h,
        fill: state === 'idle' ? FILL_IDLE : FILL_ACTIVE,
        border: state === 'idle' ? BORDER_IDLE : BORDER_ACTIVE,
    };
}

function mix(from, to, p) {
    const result = {};
    for (const key of Object.keys(to))
        result[key] = from[key] + (to[key] - from[key]) * p;
    return result;
}

class Pill {
    constructor() {
        this._themeContext = St.ThemeContext.get_for_stage(global.stage);

        this._area = new St.DrawingArea({
            reactive: false,
            can_focus: false,
            track_hover: false,
            visible: false,
        });
        this._area.connect('repaint', area => this._repaint(area));
        Main.layoutManager.addTopChrome(this._area, {
            affectsStruts: false,
            trackFullscreen: false,
        });

        this._timeline = new Clutter.Timeline({
            actor: this._area,
            duration: 1000,
            repeat_count: -1,
        });
        this._timeline.connect('new-frame', () => this._tick());

        this._present = false;
        this._stateSignals = 0;
        this._reset();

        this._themeContext.connectObject('notify::scale-factor', () => this._resize(), this);
        Main.layoutManager.connectObject('monitors-changed', () => this._place(), this);
        global.display.connectObject('workareas-changed', () => this._place(), this);
        this._resize();
    }

    destroy() {
        this._timeline.stop();
        this._themeContext.disconnectObject(this);
        Main.layoutManager.disconnectObject(this);
        global.display.disconnectObject(this);
        Main.layoutManager.removeChrome(this._area);
        this._area.destroy();
    }

    // The app took its name on the bus.
    appear() {
        console.log('Sensay pill: Sensay is running');
        this._present = true;
        this._reset();
        this._place();
        this._area.show();
        this._area.queue_repaint();

        // An app that was running before this extension started may be in
        // the middle of a dictation. A state signal that arrives before the
        // reply is newer than the reply.
        const seen = this._stateSignals;
        Gio.DBus.session.call(BUS_NAME, PILL_PATH, PILL_INTERFACE, 'GetState',
            null, new GLib.VariantType('(s)'), Gio.DBusCallFlags.NONE, -1, null,
            (connection, result) => {
                try {
                    const [state] = connection.call_finish(result).deepUnpack();
                    if (this._present && seen === this._stateSignals && STATES.includes(state))
                        this._setState(state, false);
                } catch (_) {
                    // The app serves the pill a moment after it takes the
                    // name; until then it is idle, which is what is shown.
                }
            });
    }

    // The app is gone: quit, crashed or logged out.
    vanish() {
        console.log('Sensay pill: Sensay is not running');
        this._present = false;
        this._timeline.stop();
        this._area.hide();
        this._reset();
    }

    onSignal(signal, parameters) {
        if (!this._present)
            return;
        if (signal === 'StateChanged') {
            const [state] = parameters.deepUnpack();
            this._stateSignals += 1;
            if (STATES.includes(state))
                this._setState(state, true);
        } else if (signal === 'Level') {
            const [level] = parameters.deepUnpack();
            this._levelTarget = clamp(level, 0, 1);
        }
    }

    _reset() {
        this._state = 'idle';
        this._capsule = capsuleFor('idle');
        this._animation = null;
        this._levelTarget = 0;
        this._level = 0;
        this._heights = new Array(BAR_COUNT).fill(BAR_WIDTH);
        this._barsShown = false;
        this._barMode = 'bars';
        this._dotsUntil = null;
        this._walks = [];
        this._spinnerShown = false;
        this._spinnerStart = 0;
        this._lastTick = now();
        this._frames = null;
    }

    // Frame statistics of one session, from the key going down until the
    // pill is back at rest: one line in the journal of gnome-shell.
    _countFrame(time) {
        const frames = this._frames;
        if (!frames)
            return;
        const gap = time - frames.last;
        frames.last = time;
        frames.count += 1;
        frames.longest = Math.max(frames.longest, gap);
        if (gap > 0.025)
            frames.late += 1;
    }

    _reportFrames(time) {
        const frames = this._frames;
        this._frames = null;
        if (!frames || frames.count === 0)
            return;
        const seconds = time - frames.start;
        console.log(`Sensay pill: ${frames.count} frames in ${seconds.toFixed(2)} s, ` +
            `${(frames.count / seconds).toFixed(1)} per second, longest gap ` +
            `${(frames.longest * 1000).toFixed(0)} ms, ${frames.late} gaps over 25 ms`);
    }

    _setState(next, animated) {
        if (next === this._state)
            return;
        const previous = this._state;
        this._state = next;
        const time = now();

        // Before the capsule starts to move, so nothing is seen sliding with
        // it on the way out.
        if (next !== 'waiting')
            this._spinnerShown = false;
        // The pill follows the pointer between sessions, not during one.
        if (next === 'listening' && previous === 'idle') {
            this._place();
            this._frames = {start: time, last: time, count: 0, longest: 0, late: 0};
        }

        this._moveCapsule(next, animated, time);

        switch (next) {
        case 'idle':
            this._dotsUntil = null;
            this._level = 0;
            // Bars go at once; the capsule closes around an empty inside.
            this._setBars('bars', false, time);
            break;
        case 'waiting':
            // The bars carry on as they are; the level is zero from here, so
            // they settle to their silence shape with the ripple going.
            if (previous === 'idle' || this._dotsUntil !== null) {
                this._dotsUntil = null;
                this._setBars('bars', true, time);
            }
            // Shown whole at once: the growing capsule reveals it.
            this._spinnerShown = true;
            this._spinnerStart = time;
            this._startTicking();
            break;
        case 'listening':
            this._levelTarget = 0;
            this._dotsUntil = time + EXPAND.duration + DOTS_DURATION;
            this._heights.fill(BAR_WIDTH);
            // Shown at once: the growing capsule reveals them.
            this._setBars('dots', true, time);
            this._startTicking();
            break;
        }
        this._area.queue_repaint();
    }

    _moveCapsule(state, animated, time) {
        const target = capsuleFor(state);
        if (!animated) {
            this._animation = null;
            this._capsule = target;
            return;
        }
        const spec = state === 'idle' ? COLLAPSE : EXPAND;
        this._animation = {
            from: this._presented(time),
            to: target,
            start: time,
            duration: spec.duration,
            curve: spec.curve,
        };
        this._capsule = target;
        this._startTicking();
    }

    // The capsule as it is on screen at this moment.
    _presented(time) {
        const animation = this._animation;
        if (!animation)
            return this._capsule;
        const u = (time - animation.start) / animation.duration;
        if (u >= 1) {
            this._animation = null;
            return this._capsule;
        }
        return mix(animation.from, animation.to, bezier(animation.curve, u));
    }

    // Colour and visibility of all bars at once, without animation; every
    // bar starts a fresh brightness walk.
    _setBars(mode, shown, time) {
        this._barMode = mode;
        this._barsShown = shown;
        this._walks = Array.from({length: BAR_COUNT},
            () => ({from: 1, to: 1, start: time, duration: 0.001, value: 1}));
    }

    _updateShimmer(time) {
        for (const walk of this._walks) {
            let u = (time - walk.start) / walk.duration;
            if (u >= 1) {
                walk.from = walk.to;
                walk.to = randomIn(SHIMMER_RANGE);
                walk.start = time;
                walk.duration = randomIn(SHIMMER_STEP);
                u = 0;
            }
            const eased = (1 - Math.cos(Math.PI * u)) / 2;
            walk.value = walk.from + (walk.to - walk.from) * eased;
        }
    }

    _startTicking() {
        if (this._timeline.is_playing())
            return;
        this._lastTick = now();
        this._timeline.start();
    }

    _tick() {
        const time = now();
        const dt = clamp(time - this._lastTick, 0.001, 0.1);
        this._lastTick = time;
        this._countFrame(time);

        const target = this._state === 'listening' ? this._levelTarget : 0;
        const tau = target > this._level ? LEVEL_ATTACK : LEVEL_RELEASE;
        this._level += (target - this._level) * step(dt, tau);

        if (this._dotsUntil !== null && time >= this._dotsUntil) {
            this._dotsUntil = null;
            this._setBars('bars', true, time);
        }
        if (this._dotsUntil === null && this._barsShown) {
            for (let i = 0; i < BAR_COUNT; i++)
                this._heights[i] = barTarget(i, this._level, time);
            this._updateShimmer(time);
        }

        this._presented(time);
        this._area.queue_repaint();
        if (this._state === 'idle' && !this._animation) {
            this._timeline.stop();
            this._reportFrames(time);
        }
    }

    _resize() {
        const {scaleFactor} = this._themeContext;
        this._area.set_size(Math.ceil(CANVAS.w * scaleFactor), Math.ceil(CANVAS.h * scaleFactor));
        this._place();
        this._area.queue_repaint();
    }

    // Bottom centre of the work area of the monitor the pointer is on.
    _place() {
        const index = global.display.get_current_monitor();
        if (index < 0)
            return;
        const area = Main.layoutManager.getWorkAreaForMonitor(index);
        const {scaleFactor} = this._themeContext;
        const [width, height] = this._area.get_size();
        this._area.set_position(
            Math.round(area.x + (area.width - width) / 2),
            Math.round(area.y + area.height - BOTTOM_INSET * scaleFactor - height));
    }

    _repaint(area) {
        const cr = area.get_context();
        const {scaleFactor} = this._themeContext;
        const [surfaceWidth, surfaceHeight] = area.get_surface_size();

        cr.save();
        cr.setOperator(Cairo.Operator.CLEAR);
        cr.paint();
        cr.restore();

        if (this._present) {
            cr.scale(scaleFactor, scaleFactor);
            const width = surfaceWidth / scaleFactor;
            const bottom = surfaceHeight / scaleFactor;
            const offset = (width - CANVAS.w) / 2;
            const time = now();
            const capsule = this._presented(time);
            const x = offset + capsule.left;
            const y = bottom - capsule.h;
            const radius = capsule.h / 2;

            roundedRect(cr, x, y, capsule.w, capsule.h, radius);
            cr.setSourceRGBA(0, 0, 0, capsule.fill);
            cr.fill();

            // The capsule cuts off what it contains, so it reveals the bars
            // and the spinner as it grows and hides them as it shrinks.
            cr.save();
            roundedRect(cr, x, y, capsule.w, capsule.h, radius);
            cr.clip();
            if (this._barsShown)
                this._drawBars(cr, offset + CANVAS.w / 2, bottom - capsule.h / 2);
            if (this._spinnerShown)
                this._drawSpinner(cr, x + EXPANDED.w / 2 + SPINNER_OFFSET, bottom - HEIGHT / 2, time);
            cr.restore();

            roundedRect(cr, x + 0.5, y + 0.5, capsule.w - 1, capsule.h - 1, radius - 0.5);
            cr.setLineWidth(1);
            cr.setSourceRGBA(1, 1, 1, capsule.border);
            cr.stroke();
        }
        cr.$dispose();
    }

    _drawBars(cr, centreX, centreY) {
        const left = centreX - METER_WIDTH / 2;
        for (let i = 0; i < BAR_COUNT; i++) {
            const height = this._heights[i];
            const alpha = this._barMode === 'dots' ? DOT_ALPHA : this._walks[i]?.value ?? 1;
            roundedRect(cr, left + i * BAR_PITCH, centreY - height / 2, BAR_WIDTH, height, BAR_WIDTH / 2);
            cr.setSourceRGBA(1, 1, 1, alpha);
            cr.fill();
        }
    }

    // Stepped: a new ray lights up at full brightness every step, going
    // clockwise, and each ray dims evenly until its turn comes again.
    _drawSpinner(cr, centreX, centreY, time) {
        const elapsed = Math.max(0, time - this._spinnerStart) / SPINNER_STEP;
        const steps = Math.floor(elapsed);
        const within = elapsed - steps;
        const from = SPINNER_INNER + SPINNER_THICKNESS / 2;
        const to = SPINNER_OUTER - SPINNER_THICKNESS / 2;
        cr.setLineWidth(SPINNER_THICKNESS);
        cr.setLineCap(Cairo.LineCap.ROUND);
        for (let i = 0; i < SPINNER_RAYS; i++) {
            const since = ((SPINNER_FIRST_RAY + steps - i) % SPINNER_RAYS + SPINNER_RAYS) % SPINNER_RAYS;
            const alpha = Math.max(0, 1 - SPINNER_FADE_PER_STEP * (since + within));
            // Counted clockwise from straight up.
            const angle = i * 2 * Math.PI / SPINNER_RAYS;
            const dx = Math.sin(angle);
            const dy = -Math.cos(angle);
            cr.moveTo(centreX + dx * from, centreY + dy * from);
            cr.lineTo(centreX + dx * to, centreY + dy * to);
            cr.setSourceRGBA(1, 1, 1, alpha);
            cr.stroke();
        }
    }
}

export default class SensayPillExtension extends Extension {
    enable() {
        this._pill = new Pill();
        this._paster = new Paster();
        // Unique bus name of the running app; only it may paste.
        this._appOwner = null;

        this._signals = Gio.DBus.session.signal_subscribe(
            null, PILL_INTERFACE, null, PILL_PATH, null, Gio.DBusSignalFlags.NONE,
            (_connection, _sender, _path, _interface, signal, parameters) =>
                this._pill?.onSignal(signal, parameters));
        this._watch = Gio.bus_watch_name(Gio.BusType.SESSION, BUS_NAME,
            Gio.BusNameWatcherFlags.NONE,
            (_connection, _name, owner) => {
                this._appOwner = owner;
                this._pill?.appear();
            },
            () => {
                this._appOwner = null;
                this._pill?.vanish();
            });

        this._shell = Gio.DBusExportedObject.wrapJSObject(SHELL_INTERFACE, {
            PasteAsync: ([text], invocation) => this._onPaste(text, invocation),
        });
        this._shell.export(Gio.DBus.session, SHELL_PATH);
    }

    disable() {
        this._shell.unexport();
        this._shell = null;
        Gio.bus_unwatch_name(this._watch);
        Gio.DBus.session.signal_unsubscribe(this._signals);
        this._paster.destroy();
        this._paster = null;
        this._pill.destroy();
        this._pill = null;
        this._appOwner = null;
    }

    _onPaste(text, invocation) {
        // Pressing keys in the focused window is not offered to just anyone
        // on the bus.
        if (!this._appOwner || invocation.get_sender() !== this._appOwner) {
            invocation.return_dbus_error('com.advertumtech.Sensay.Error.Denied',
                'Only the running Sensay app may paste.');
            return;
        }
        this._paster.paste(text).then(
            report => invocation.return_value(new GLib.Variant('(s)', [JSON.stringify(report)])),
            error => invocation.return_dbus_error('com.advertumtech.Sensay.Error.Failed',
                String(error?.message ?? error)));
    }
}
