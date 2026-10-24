// The two windows, Settings and History, drawn with egui through eframe. They
// live on the main thread, which winit wants; everything else runs on tokio.
//
// No window is open at launch. The first request starts an eframe run with
// that window as its root; the other window opens beside it as a second
// viewport. Closing the root while the other window is open hands the root
// over to that window, so either can be closed first. When both are closed
// the run returns, and the thread waits for the next request. The event loop
// is kept between runs (eframe's run_and_return).
//
// A window that cannot open never takes the app down. The libraries winit
// loads while the program runs are looked for before every opening
// (libraries.rs), and a panic inside a run is caught; after one the windows
// stay closed until Sensay restarts, and dictation goes on.

mod about;
mod fonts;
mod frame;
mod history;
mod libraries;
mod settings;

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::Command;
use crate::asr::recogniser::Recogniser;
use crate::audio::Inputs;
use crate::config::Config;
use crate::desktop;
use crate::history::History;
use crate::log;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Settings,
    History,
    About,
}

pub enum UiCommand {
    /// The app is up; what the windows reach into.
    Ready(Box<Bridge>),
    Show(WindowKind),
    Quit,
}

/// Everything the windows reach into, handed over once the app has started.
pub struct Bridge {
    pub runtime: tokio::runtime::Handle,
    /// The settings as the app runs with them. The Settings window changes
    /// them here and tells the app with Command::SettingsChanged.
    pub config: Arc<Mutex<Config>>,
    pub inputs: watch::Receiver<Inputs>,
    pub history: Option<History>,
    pub recogniser: Recogniser,
    pub commands: UnboundedSender<Command>,
    /// For asking the desktop whether windows should be dark.
    pub connection: Option<zbus::Connection>,
}

/// Requests from the app, and the egui context to wake while a run is on.
struct Mailbox {
    queue: Mutex<VecDeque<UiCommand>>,
    arrived: Condvar,
    context: Mutex<Option<egui::Context>>,
}

impl Mailbox {
    fn post(&self, command: UiCommand) {
        self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push_back(command);
        self.arrived.notify_one();
        if let Some(context) = self.context.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
            context.request_repaint();
        }
    }

    fn wait(&self) -> UiCommand {
        let mut queue = self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(command) = queue.pop_front() {
                return command;
            }
            queue = self.arrived.wait(queue).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn take(&self) -> Option<UiCommand> {
        self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).pop_front()
    }

    fn set_context(&self, context: Option<egui::Context>) {
        *self.context.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = context;
    }
}

/// winit takes the windows' scale on X11 from Xft.dpi alone, which Xfce at
/// 2x leaves at 96, so it is told the scale the desktop asks for, unless
/// WINIT_X11_SCALE_FACTOR is set already. Called before any other thread
/// starts.
pub fn set_x11_scale() {
    if std::env::var_os("WINIT_X11_SCALE_FACTOR").is_some() || !desktop::windows_use_x11() {
        return;
    }
    let Some(scale) = crate::x11_scale::screen() else { return };
    // SAFETY: no other thread runs yet, so nothing reads the environment
    // while it changes.
    unsafe { std::env::set_var("WINIT_X11_SCALE_FACTOR", scale.value.to_string()) };
    log::info!(App, "windows: X11 {}", scale.describe());
}

/// Runs on the main thread until the app quits.
pub fn run(commands: Receiver<UiCommand>) {
    let mailbox = Arc::new(Mailbox {
        queue: Mutex::new(VecDeque::new()),
        arrived: Condvar::new(),
        context: Mutex::new(None),
    });
    {
        let mailbox = mailbox.clone();
        let forwarding = std::thread::Builder::new()
            .name("ui-mail".to_string())
            .spawn(move || {
                while let Ok(command) = commands.recv() {
                    mailbox.post(command);
                }
                mailbox.post(UiCommand::Quit);
            });
        if let Err(error) = forwarding {
            log::error!(App, "windows: could not start the mail thread ({error}), no windows");
            return;
        }
    }

    let mut bridge: Option<Arc<Bridge>> = None;
    // Set once a run has panicked: winit is not used again until a restart.
    let mut broken: Option<String> = None;
    loop {
        match mailbox.wait() {
            UiCommand::Ready(ready) => bridge = Some(Arc::from(ready)),
            UiCommand::Quit => return,
            UiCommand::Show(kind) => {
                let Some(bridge) = bridge.clone() else {
                    log::warning!(App, "windows: {kind:?} asked for before the app was ready");
                    continue;
                };
                if let Some(reason) = &broken {
                    log::error!(
                        App,
                        "windows: {kind:?} not opened, the windows failed earlier ({reason}); they open again after a restart"
                    );
                    continue;
                }
                if let Some(reason) = libraries::missing() {
                    log::error!(App, "windows: {kind:?} not opened, {reason}");
                    continue;
                }
                match panic::catch_unwind(AssertUnwindSafe(|| open(kind, bridge, &mailbox))) {
                    Ok(Outcome::Quit) => return,
                    Ok(Outcome::Closed) => {}
                    Err(payload) => {
                        mailbox.set_context(None);
                        let reason = panic_text(payload.as_ref());
                        log::error!(
                            App,
                            "windows: failed ({reason}), no windows until Sensay restarts; dictation goes on"
                        );
                        broken = Some(reason);
                    }
                }
            }
        }
    }
}

/// The message a panic was raised with.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_string())
}

#[derive(PartialEq, Eq)]
enum Outcome {
    Closed,
    Quit,
}

/// One eframe run, from the first window opening to the last one closing.
fn open(kind: WindowKind, bridge: Arc<Bridge>, mailbox: &Arc<Mailbox>) -> Outcome {
    let quit = Arc::new(Mutex::new(false));
    let options = eframe::NativeOptions {
        viewport: viewport(kind),
        run_and_return: true,
        // In the middle of the screen, as on macOS; on X11 the window manager
        // would pick a free corner. Wayland compositors place windows
        // themselves.
        centered: true,
        ..Default::default()
    };
    let app_mailbox = mailbox.clone();
    let app_quit = quit.clone();
    // Asked at every opening, so a change of the desktop's style shows the
    // next time a window opens.
    let dark = bridge
        .connection
        .as_ref()
        .and_then(|connection| bridge.runtime.block_on(desktop::prefers_dark(connection)))
        .unwrap_or(false);
    let accent = bridge
        .connection
        .as_ref()
        .and_then(|connection| bridge.runtime.block_on(desktop::accent_color(connection)))
        .map(|[red, green, blue]| {
            let channel = |value: f64| (value * 255.0).round() as u8;
            egui::Color32::from_rgb(channel(red), channel(green), channel(blue))
        });
    log::info!(App, "windows: opening {kind:?}, {} style", if dark { "dark" } else { "light" });
    let result = eframe::run_native(
        "Sensay",
        options,
        Box::new(move |creation| {
            let context = creation.egui_ctx.clone();
            context.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
            fonts::install(&context);
            if let Some(accent) = accent {
                context.data_mut(|data| data.insert_temp(accent_id(), accent));
            }
            app_mailbox.set_context(Some(context.clone()));
            Ok(Box::new(Windows::new(kind, bridge, app_mailbox, app_quit, context)))
        }),
    );
    mailbox.set_context(None);
    if let Err(error) = result {
        log::error!(App, "windows: {error}");
    }
    if *quit.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        Outcome::Quit
    } else {
        Outcome::Closed
    }
}

/// Where the desktop's accent colour is kept for the windows to read.
pub fn accent_id() -> egui::Id {
    egui::Id::new("sensay-accent")
}

fn viewport(kind: WindowKind) -> egui::ViewportBuilder {
    // The windows draw their own frame (frame.rs), rounded like GNOME's.
    let builder = egui::ViewportBuilder::default()
        .with_app_id("com.advertumtech.Sensay")
        .with_decorations(false)
        .with_transparent(true);
    match kind {
        // The height follows the content (`fit`).
        WindowKind::Settings => builder
            .with_title("Settings")
            .with_inner_size([640.0, 700.0])
            .with_min_inner_size([460.0, 200.0]),
        WindowKind::History => builder
            .with_title("History")
            .with_inner_size([880.0, 620.0])
            .with_min_inner_size([620.0, 360.0]),
        // The height follows the content (`fit`).
        WindowKind::About => builder
            .with_title("About Sensay")
            .with_inner_size([380.0, 240.0])
            .with_min_inner_size([320.0, 160.0]),
    }
}

struct Windows {
    root: WindowKind,
    second: Option<WindowKind>,
    /// Where the second window opens: over the middle of the first one.
    /// Kept as it was at opening, so the second window does not follow the
    /// first when that one moves.
    second_at: Option<egui::Pos2>,
    settings: settings::SettingsWindow,
    history: history::HistoryWindow,
    about: about::AboutWindow,
    bridge: Arc<Bridge>,
    mailbox: Arc<Mailbox>,
    quit: Arc<Mutex<bool>>,
}

impl Windows {
    fn new(
        root: WindowKind,
        bridge: Arc<Bridge>,
        mailbox: Arc<Mailbox>,
        quit: Arc<Mutex<bool>>,
        context: egui::Context,
    ) -> Windows {
        wake_on_changes(&bridge, &context);
        Windows {
            root,
            second: None,
            second_at: None,
            settings: settings::SettingsWindow::new(),
            history: history::HistoryWindow::new(),
            about: about::AboutWindow::new(),
            bridge,
            mailbox,
            quit,
        }
    }

    fn read_mail(&mut self, context: &egui::Context) {
        while let Some(command) = self.mailbox.take() {
            match command {
                UiCommand::Show(kind) if kind == self.root => {
                    context.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                UiCommand::Show(kind) if Some(kind) == self.second => {
                    context.send_viewport_cmd_to(viewport_id(kind), egui::ViewportCommand::Focus);
                }
                UiCommand::Show(kind) => {
                    log::info!(App, "windows: opening {kind:?}");
                    self.reset(kind);
                    self.second = Some(kind);
                    let outer = context.input(|input| input.viewport().outer_rect);
                    self.second_at = outer
                        .zip(viewport(kind).inner_size)
                        .map(|(outer, size)| outer.center() - size / 2.0);
                }
                UiCommand::Quit => {
                    *self.quit.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                UiCommand::Ready(_) => {}
            }
        }
    }

    /// A window that opens again starts from what is stored, not from what it
    /// showed last time.
    fn reset(&mut self, kind: WindowKind) {
        match kind {
            WindowKind::Settings => self.settings = settings::SettingsWindow::new(),
            WindowKind::History => self.history = history::HistoryWindow::new(),
            WindowKind::About => self.about = about::AboutWindow::new(),
        }
    }

    fn draw(&mut self, kind: WindowKind, ui: &mut egui::Ui) {
        let (title, resizable, view) = match kind {
            WindowKind::Settings => ("Settings", false, false),
            WindowKind::History => ("History", true, true),
            WindowKind::About => ("About Sensay", false, false),
        };
        let (content, close) = frame::window(ui, title, resizable, view);
        if close {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(content));
        match kind {
            WindowKind::Settings => {
                let height = self.settings.ui(&mut inner, &self.bridge);
                fit(ui.ctx(), height + frame::HEADER_HEIGHT);
            }
            WindowKind::History => self.history.ui(&mut inner, &self.bridge),
            WindowKind::About => {
                let height = self.about.ui(&mut inner);
                fit(ui.ctx(), height + frame::HEADER_HEIGHT);
            }
        }
    }
}

/// The window as tall as its content, as the Settings window is on macOS:
/// the microphone list grows and shrinks as devices come and go.
fn fit(context: &egui::Context, content_height: f32) {
    let wanted = content_height.ceil();
    let Some(inner) = context.input(|input| input.viewport().inner_rect) else { return };
    if (inner.height() - wanted).abs() > 1.0 {
        context.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(inner.width(), wanted)));
    }
}

/// No fill and no margin: the frame (frame.rs) paints the whole window.
fn panel(_kind: WindowKind) -> egui::CentralPanel {
    egui::CentralPanel::default().frame(egui::Frame::new())
}

fn viewport_id(kind: WindowKind) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(match kind {
        WindowKind::Settings => "sensay-settings",
        WindowKind::History => "sensay-history",
        WindowKind::About => "sensay-about",
    })
}

/// Repaints when something a window shows changes on the app's side: the
/// microphones, the history.
fn wake_on_changes(bridge: &Bridge, context: &egui::Context) {
    let mut inputs = bridge.inputs.clone();
    let wake = context.clone();
    bridge.runtime.spawn(async move {
        while inputs.changed().await.is_ok() {
            wake.request_repaint();
        }
    });
    if let Some(history) = &bridge.history {
        let mut changes = history.watch();
        let wake = context.clone();
        bridge.runtime.spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => wake.request_repaint(),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }
}

impl eframe::App for Windows {
    /// Transparent outside the rounded frame.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.read_mail(&context);

        // The root closing while the other window is open: the root takes
        // the other window over.
        let quitting = *self.quit.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !quitting && context.input(|input| input.viewport().close_requested()) {
            if let Some(other) = self.second.take() {
                context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.root = other;
                let builder = viewport(other);
                if let Some(title) = builder.title.clone() {
                    context.send_viewport_cmd(egui::ViewportCommand::Title(title));
                }
                if let Some(size) = builder.inner_size {
                    context.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                }
                if let Some(size) = builder.min_inner_size {
                    context.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(size));
                }
            }
        }

        let root = self.root;
        panel(root).show(ui, |ui| {
            self.draw(root, ui);
        });

        if let Some(kind) = self.second {
            let mut closed = false;
            let mut builder = viewport(kind);
            if let Some(at) = self.second_at {
                builder = builder.with_position(at);
            }
            context.show_viewport_immediate(viewport_id(kind), builder, |ui, _class| {
                panel(kind).show(ui, |ui| {
                    self.draw(kind, ui);
                });
                if ui.ctx().input(|input| input.viewport().close_requested()) {
                    closed = true;
                }
            });
            if closed {
                self.second = None;
            }
        }
    }
}
