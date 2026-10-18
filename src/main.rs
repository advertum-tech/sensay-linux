// Sensay for Linux: hold the activation key, speak, let go, and the text is
// pasted into the window under the cursor. The macOS version is the model
// for the behavior.
//
//   sensay            run (a second launch says so and exits)
//   sensay settings   open the settings of the running app
//   sensay history    open the history of the running app
//   sensay setup      one-time system setup (step 8)
//
// Threads: the main thread draws the Settings and History windows (eframe,
// which winit wants on the main thread); tokio runs everything else on its
// own threads (D-Bus, tray, recognition, signals); the activation key is read
// on a std thread of its own; sound is recorded on a std thread named audio
// (libpulse).

mod asr;
mod audio;
mod autostart;
mod bus;
mod config;
mod controller;
mod desktop;
mod hotkey;
mod history;
mod log;
mod paste;
mod paths;
mod pill;
mod tray;
mod ui;

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use zbus::Connection;

use crate::audio::AudioRecorder;
use crate::asr::ServiceSettings;
use crate::asr::recogniser::Recogniser;
use crate::controller::DictationController;
use crate::history::History;
use crate::paste::TextSink;
use crate::pill::Pill;
use crate::hotkey::{Hotkey, HotkeyEvent};
use crate::ui::{Bridge, UiCommand, WindowKind};

/// What the tray menu, the D-Bus methods and the windows ask of the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    OpenSettings,
    OpenHistory,
    /// The Settings window changed the shared settings.
    SettingsChanged,
    Quit,
}

enum Mode {
    Run,
    Settings,
    History,
    Setup,
}

const USAGE: &str = "usage: sensay [settings | history | setup]";

fn main() -> ExitCode {
    let mode = match parse_args(std::env::args_os().skip(1)) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("sensay: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    if let Mode::Setup = mode {
        println!("not implemented yet");
        return ExitCode::SUCCESS;
    }

    log::init();

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!(App, "could not start the tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let (ui_tx, ui_rx) = std::sync::mpsc::channel();
    let app = runtime.spawn(async move {
        let code = run(mode, ui_tx.clone()).await;
        let _ = ui_tx.send(UiCommand::Quit);
        code
    });
    ui::run(ui_rx);
    let code = runtime.block_on(app).unwrap_or(ExitCode::FAILURE);
    runtime.shutdown_timeout(Duration::from_millis(500));
    code
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Mode, String> {
    let Some(first) = args.next() else {
        return Ok(Mode::Run);
    };
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument {}", extra.to_string_lossy()));
    }
    match first.to_str() {
        Some("settings") => Ok(Mode::Settings),
        Some("history") => Ok(Mode::History),
        Some("setup") => Ok(Mode::Setup),
        _ => Err(format!("unknown command {}", first.to_string_lossy())),
    }
}

async fn run(mode: Mode, ui: std::sync::mpsc::Sender<UiCommand>) -> ExitCode {
    let (commands_tx, mut commands) = mpsc::unbounded_channel();

    let connection = match bus::claim(commands_tx.clone()).await {
        Ok(bus::Instance::First(connection)) => Some(connection),
        Ok(bus::Instance::Second(connection)) => return hand_over(&connection, mode).await,
        Err(error) => {
            log::error!(
                App,
                "session bus unavailable ({error}): running without the single-instance check"
            );
            None
        }
    };

    // Installed before anything slow, so Ctrl+C in the terminal always goes
    // through the shutdown below.
    let mut sigint = listen(SignalKind::interrupt(), "SIGINT");
    let mut sigterm = listen(SignalKind::terminate(), "SIGTERM");

    log::info!(
        App,
        "Sensay {} started, pid {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );

    let config = config::load();
    log::info!(
        App,
        "settings: speech_service={} activation_key={} microphone={} api_key={}",
        config.speech_service.as_str(),
        config.activation_key,
        config.microphone,
        if config.api_key.is_empty() { "empty" } else { "set" }
    );
    log::info!(
        App,
        "backends: hotkey={} audio={} pill={} clipboard={} keys={}",
        Hotkey::backend_name(),
        AudioRecorder::backend_name(),
        Pill::backend_name(),
        TextSink::backend_names().0,
        TextSink::backend_names().1
    );

    let (hotkey_tx, mut hotkey_events) = mpsc::unbounded_channel();
    let mut hotkey = Some(Hotkey::start(config.activation_key, hotkey_tx.clone()));

    let recorder = AudioRecorder::open().await;
    let pill = Pill::start(connection.as_ref(), recorder.level()).await;

    let tray = tokio::spawn(tray::start(commands_tx.clone()));

    let history = match History::open().await {
        Ok(history) => {
            log::info!(
                Storage,
                "history open at {}",
                paths::database_path().map(|path| path.display().to_string()).unwrap_or_default()
            );
            history.mark_interrupted().await;
            // At launch, one of the two moments the sweep runs; never on the
            // dictation path.
            tokio::spawn(history::sweep(history.clone()));
            Some(history)
        }
        Err(error) => {
            log::error!(Storage, "could not open the history: {error}");
            None
        }
    };

    // Read by the queue at the moment each row is sent; the Settings window
    // changes it through the shared settings.
    let (service_settings_tx, service_settings) = tokio::sync::watch::channel(ServiceSettings {
        service: config.speech_service,
        api_key: config.api_key.clone(),
    });
    let recogniser = Recogniser::new(service_settings);
    let sink = TextSink::new(connection.as_ref());

    // The settings as the app runs with them: the Settings window writes
    // here, and `applied` is what the parts of the app were last told.
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let mut applied = config.clone();

    let _ = ui.send(UiCommand::Ready(Box::new(Bridge {
        runtime: tokio::runtime::Handle::current(),
        config: shared_config.clone(),
        inputs: recorder.inputs(),
        history: history.clone(),
        recogniser: recogniser.clone(),
        sink: sink.clone(),
        commands: commands_tx.clone(),
        connection: connection.clone(),
    })));

    let (controller_tx, mut controller_events) = mpsc::unbounded_channel();
    let mut controller = DictationController::new(
        recorder,
        config.microphone,
        pill,
        controller_tx,
        history,
        recogniser,
        sink,
    );

    // `sensay settings` or `sensay history` with no app running: this process
    // becomes the app and does what was asked.
    match mode {
        Mode::Settings => {
            let _ = commands_tx.send(Command::OpenSettings);
        }
        Mode::History => {
            let _ = commands_tx.send(Command::OpenHistory);
        }
        Mode::Run | Mode::Setup => {}
    }

    loop {
        tokio::select! {
            Some(event) = hotkey_events.recv() => match event {
                HotkeyEvent::Down => controller.key_down().await,
                HotkeyEvent::Up => controller.key_up().await,
            },
            Some(event) = controller_events.recv() => controller.handle(event),
            Some(command) = commands.recv() => match command {
                Command::OpenSettings => {
                    let _ = ui.send(UiCommand::Show(WindowKind::Settings));
                }
                Command::OpenHistory => {
                    let _ = ui.send(UiCommand::Show(WindowKind::History));
                }
                Command::SettingsChanged => {
                    let wanted = shared_config.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
                    if wanted.speech_service != applied.speech_service || wanted.api_key != applied.api_key {
                        log::info!(
                            App,
                            "settings: speech_service={} api_key={}",
                            wanted.speech_service.as_str(),
                            if wanted.api_key.is_empty() { "empty" } else { "set" }
                        );
                        service_settings_tx.send_replace(ServiceSettings {
                            service: wanted.speech_service,
                            api_key: wanted.api_key.clone(),
                        });
                    }
                    if wanted.activation_key != applied.activation_key {
                        log::info!(App, "settings: activation_key={}, restarting the key listener", wanted.activation_key);
                        if let Some(old) = hotkey.take() {
                            if tokio::task::spawn_blocking(move || old.stop()).await.is_err() {
                                log::error!(Hotkey, "stopping the hotkey thread failed");
                            }
                        }
                        hotkey = Some(Hotkey::start(wanted.activation_key, hotkey_tx.clone()));
                    }
                    if wanted.microphone != applied.microphone {
                        log::info!(App, "settings: microphone={}", wanted.microphone);
                        controller.set_microphone(wanted.microphone.clone());
                    }
                    applied = wanted;
                }
                Command::Quit => {
                    log::info!(App, "quit requested");
                    break;
                }
            },
            _ = receive(&mut sigint) => {
                log::info!(App, "SIGINT received");
                break;
            }
            _ = receive(&mut sigterm) => {
                log::info!(App, "SIGTERM received");
                break;
            }
        }
    }

    log::info!(App, "Sensay terminating");

    if let Some(hotkey) = hotkey.take() {
        if tokio::task::spawn_blocking(move || hotkey.stop()).await.is_err() {
            log::error!(Hotkey, "stopping the hotkey thread failed");
        }
    }

    if tokio::task::spawn_blocking(move || controller.shut_down()).await.is_err() {
        log::error!(Audio, "stopping the audio thread failed");
    }

    if tray.is_finished() {
        if let Ok(Some(handle)) = tray.await {
            handle.shutdown().await;
        }
    } else {
        // Still retrying for a tray host.
        tray.abort();
    }

    if let Some(connection) = &connection {
        bus::release(connection).await;
    }

    ExitCode::SUCCESS
}

/// Another process owns the name: pass the command on and exit.
async fn hand_over(connection: &Connection, mode: Mode) -> ExitCode {
    let command = match mode {
        Mode::Settings => Command::OpenSettings,
        Mode::History => Command::OpenHistory,
        Mode::Run | Mode::Setup => {
            eprintln!("sensay: already running");
            return ExitCode::SUCCESS;
        }
    };
    match bus::send(connection, command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sensay: could not reach the running instance: {error}");
            ExitCode::FAILURE
        }
    }
}

fn listen(kind: SignalKind, name: &str) -> Option<Signal> {
    match signal(kind) {
        Ok(signal) => Some(signal),
        Err(error) => {
            log::error!(App, "cannot handle {name}: {error}");
            None
        }
    }
}

/// Resolves on the next signal; never, when the handler could not be set.
async fn receive(signal: &mut Option<Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}
