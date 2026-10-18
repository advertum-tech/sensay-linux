// The two windows, Settings and History, drawn with egui through eframe. They
// live on the main thread, which winit wants; everything else runs on tokio.
//
// No window is open at launch. The first request starts an eframe run with
// that window as its root; the other window opens beside it as a second
// viewport. Closing the root while the other window is open hands the root
// over to that window, so either can be closed first. When both are closed
// the run returns, and the thread waits for the next request. The event loop
// is kept between runs (eframe's run_and_return).

mod fonts;
mod frame;
mod history;
mod settings;

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
use crate::paste::TextSink;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Settings,
    History,
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
    pub sink: TextSink,
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
    loop {
        match mailbox.wait() {
            UiCommand::Ready(ready) => bridge = Some(Arc::from(ready)),
            UiCommand::Quit => return,
            UiCommand::Show(kind) => {
                let Some(bridge) = bridge.clone() else {
                    log::warning!(App, "windows: {kind:?} asked for before the app was ready");
                    continue;
                };
                if open(kind, bridge, &mailbox) == Outcome::Quit {
                    return;
                }
            }
        }
    }
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
    }
}

/// What a window asks of the app after a frame.
pub enum Action {
    /// Close the History window, then paste this text where the cursor is.
    PasteAndClose { text: String, id: i64 },
}

struct Windows {
    root: WindowKind,
    second: Option<WindowKind>,
    settings: settings::SettingsWindow,
    history: history::HistoryWindow,
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
            settings: settings::SettingsWindow::new(),
            history: history::HistoryWindow::new(),
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
        }
    }

    fn draw(&mut self, kind: WindowKind, ui: &mut egui::Ui) -> Option<Action> {
        let (title, resizable, view) = match kind {
            WindowKind::Settings => ("Settings", false, false),
            WindowKind::History => ("History", true, true),
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
                None
            }
            WindowKind::History => self.history.ui(&mut inner, &self.bridge),
        }
    }

    fn act(&mut self, action: Action, context: &egui::Context) {
        match action {
            Action::PasteAndClose { text, id } => {
                self.close(WindowKind::History, context);
                let sink = self.bridge.sink.clone();
                self.bridge.runtime.spawn(async move {
                    // Long enough for the focus to go back to the window the
                    // user was in before the history.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if let Err(error) = sink.deliver(&text, &format!("row{id}")).await {
                        log::error!(Paste, "[row{id}] {error}");
                    }
                });
            }
        }
    }

    fn close(&mut self, kind: WindowKind, context: &egui::Context) {
        if self.second == Some(kind) {
            self.second = None;
        } else if self.root == kind {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
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
        let mut action = None;
        panel(root).show(ui, |ui| {
            action = self.draw(root, ui);
        });
        if let Some(action) = action {
            self.act(action, &context);
        }

        if let Some(kind) = self.second {
            let mut closed = false;
            let mut action = None;
            context.show_viewport_immediate(viewport_id(kind), viewport(kind), |ui, _class| {
                panel(kind).show(ui, |ui| {
                    action = self.draw(kind, ui);
                });
                if ui.ctx().input(|input| input.viewport().close_requested()) {
                    closed = true;
                }
            });
            if closed {
                self.second = None;
            }
            if let Some(action) = action {
                self.act(action, &context);
            }
        }
    }
}
