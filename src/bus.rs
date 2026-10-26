// The app's name on the session bus. Owning it is what makes this process the
// one running Sensay; a second launch finds it taken, hands its command to the
// owner and exits.

use tokio::sync::mpsc::UnboundedSender;
use zbus::Connection;
use zbus::fdo::RequestNameFlags;

use crate::Command;
use crate::log;

pub const NAME: &str = "com.advertumtech.Sensay";
const PATH: &str = "/com/advertumtech/Sensay";

/// Methods other processes call on the running app.
struct AppInterface {
    commands: UnboundedSender<Command>,
}

impl AppInterface {
    fn forward(&self, command: Command) {
        log::info!(App, "D-Bus: {command:?} called");
        // The receiver is gone only while the app is shutting down.
        let _ = self.commands.send(command);
    }
}

#[zbus::interface(name = "com.advertumtech.Sensay")]
impl AppInterface {
    fn open_settings(&self) {
        self.forward(Command::OpenSettings);
    }

    fn open_history(&self) {
        self.forward(Command::OpenHistory);
    }

    fn open_about(&self) {
        self.forward(Command::OpenAbout);
    }

    fn quit(&self) {
        self.forward(Command::Quit);
    }
}

#[zbus::proxy(
    interface = "com.advertumtech.Sensay",
    default_service = "com.advertumtech.Sensay",
    default_path = "/com/advertumtech/Sensay"
)]
trait App {
    fn open_settings(&self) -> zbus::Result<()>;
    fn open_history(&self) -> zbus::Result<()>;
    fn open_about(&self) -> zbus::Result<()>;
}

pub enum Instance {
    /// This process owns the name and serves the methods above.
    First(Connection),
    /// Another process owns the name.
    Second(Connection),
}

/// Connects to the session bus and tries to take the name. The methods are
/// served before the name is asked for, so a call that arrives the moment
/// the name is taken is not lost.
pub async fn claim(commands: UnboundedSender<Command>) -> zbus::Result<Instance> {
    let connection = zbus::connection::Builder::session()?
        .serve_at(PATH, AppInterface { commands })?
        .build()
        .await?;

    // Without DoNotQueue a second process would wait in line for the name.
    match connection
        .request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(_) => Ok(Instance::First(connection)),
        Err(zbus::Error::NameTaken) => Ok(Instance::Second(connection)),
        Err(error) => Err(error),
    }
}

/// Asks the running app to open a window. Used by `sensay settings` and
/// `sensay history` when another process owns the name.
pub async fn send(connection: &Connection, command: Command) -> zbus::Result<()> {
    let app = AppProxy::new(connection).await?;
    match command {
        Command::OpenSettings => app.open_settings().await,
        Command::OpenHistory => app.open_history().await,
        Command::OpenAbout => app.open_about().await,
        Command::SettingsChanged | Command::TestError(_) | Command::Quit => Ok(()),
    }
}

/// Gives the name up, so a new launch can take it at once.
pub async fn release(connection: &Connection) {
    if let Err(error) = connection.release_name(NAME).await {
        log::warning!(App, "could not release {NAME}: {error}");
    }
}
