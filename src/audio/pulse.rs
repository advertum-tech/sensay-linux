// The audio thread: one libpulse connection, the list of microphones, and at
// most one recording stream.
//
// libpulse objects are not Send, so all of them are created, used and dropped
// here. The thread runs libpulse's standard main loop; commands arrive over a
// channel, and a byte on a pipe the main loop watches wakes it for them.
//
// Nothing is read from the stream inside libpulse callbacks: after every pass
// of the main loop the stream is drained by `peek`, which keeps the stream
// and the file out of shared ownership.
//
// The stream is closed with `disconnect`, never corked, when the key comes up:
// a Bluetooth headset leaves call mode only when no stream records from it.

use std::cell::{Cell, RefCell};
use std::io::PipeReader;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::Instant;

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{Introspector, ServerInfo, SourceInfo};
use pulse::context::subscribe::{Facility, InterestMaskSet, Operation as Change};
use pulse::context::{Context, FlagSet as ContextFlags, State as ContextState};
use pulse::def::BufferAttr;
use pulse::mainloop::api::Mainloop as _;
use pulse::mainloop::events::io::FlagSet as IoFlags;
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::{Proplist, properties};
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlags, PeekResult, State as StreamState, Stream};
use tokio::sync::{oneshot, watch};

use super::sink::{SAMPLE_RATE, Sink};
use super::{AudioError, Command, Input, Inputs, Recording};
use crate::config::Microphone;
use crate::log;

const APP_NAME: &str = "Sensay";
const APP_ID: &str = "com.advertumtech.Sensay";
const STREAM_NAME: &str = "Dictation";

/// Requested size of one fragment the server sends: 20 ms of S16LE mono at
/// 16 kHz. The server may round it.
const FRAGMENT_BYTES: u32 = SAMPLE_RATE / 50 * 2;

const SAMPLE_SPEC: Spec = Spec {
    format: Format::S16le,
    rate: SAMPLE_RATE,
    channels: 1,
};

/// Everything the thread is handed by AudioRecorder::open.
pub struct Channels {
    pub commands: Receiver<Command>,
    pub wake: PipeReader,
    pub level: watch::Sender<f32>,
    pub inputs: watch::Sender<Inputs>,
    pub ready: oneshot::Sender<()>,
}

pub fn spawn(channels: Channels) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("audio".to_string())
        .spawn(move || run(channels))
}

fn run(channels: Channels) {
    let Channels {
        commands,
        wake,
        level,
        inputs,
        ready,
    } = channels;

    let Some(mut mainloop) = Mainloop::new() else {
        log::error!(Audio, "could not create the libpulse main loop, recording is off");
        let _ = ready.send(());
        serve_without_server(&commands);
        return;
    };

    // The wake pipe is read until empty each time it fires; the commands
    // themselves are taken from the channel after the main loop returns.
    let wake_fd = wake.as_raw_fd();
    set_nonblocking(wake_fd);
    let wake_event = mainloop.new_io_event(
        wake_fd,
        IoFlags::INPUT,
        Box::new(|_, fd, _| drain_pipe(fd)),
    );
    if wake_event.is_none() {
        log::error!(Audio, "could not watch the wake pipe, recording is off");
        let _ = ready.send(());
        serve_without_server(&commands);
        return;
    }

    let mut engine = Engine {
        mainloop,
        context: None,
        inventory: Rc::new(RefCell::new(Inventory {
            inputs: Inputs::default(),
            publish: inputs,
        })),
        capture: None,
        level,
        connection_lost_logged: false,
    };
    engine.connect();
    let _ = ready.send(());

    'main: loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Quit) | Err(TryRecvError::Disconnected) => break 'main,
                Ok(command) => engine.handle(command),
                Err(TryRecvError::Empty) => break,
            }
        }

        match engine.mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => break,
            IterateResult::Err(error) => {
                log::error!(Audio, "libpulse main loop failed ({error}), recording is off");
                break;
            }
        }

        engine.service_capture();
        engine.check_context();
    }

    engine.shut_down();
    // Kept alive until here: dropping it removes the watch on the pipe.
    drop(wake_event);
    drop(wake);
}

/// Answers commands with an error until told to quit, for when libpulse could
/// not even be set up.
fn serve_without_server(commands: &Receiver<Command>) {
    let unavailable = || AudioError::CouldNotStart("libpulse is not available".to_string());
    while let Ok(command) = commands.recv() {
        match command {
            Command::Start { reply, .. } => {
                let _ = reply.send(Err(unavailable()));
            }
            Command::Stop { reply } => {
                let _ = reply.send(Err(unavailable()));
            }
            Command::Cancel => {}
            Command::Quit => return,
        }
    }
}

fn set_nonblocking(fd: i32) {
    // SAFETY: fcntl on a descriptor this thread owns; no memory is passed.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn drain_pipe(fd: i32) {
    let mut buffer = [0u8; 64];
    loop {
        // SAFETY: reads into a local buffer of the given length.
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read <= 0 {
            break;
        }
    }
}

/// The microphones as last heard from the server, published for the Settings
/// window on every change.
struct Inventory {
    inputs: Inputs,
    publish: watch::Sender<Inputs>,
}

impl Inventory {
    fn publish(&self) {
        self.publish.send_replace(self.inputs.clone());
    }

    /// Adds or refreshes a source. Only a new one is logged: a source reports
    /// a change every time it starts or stops running.
    fn upsert(&mut self, input: Input) {
        match self.inputs.list.iter().position(|known| known.index == input.index) {
            Some(position) => {
                if self.inputs.list[position] == input {
                    return;
                }
                self.inputs.list[position] = input;
            }
            None => {
                log::info!(Audio, "source added: {}", describe(&input));
                self.inputs.list.push(input);
            }
        }
        self.publish();
    }

    fn remove(&mut self, index: u32) {
        if let Some(position) = self.inputs.list.iter().position(|known| known.index == index) {
            let input = self.inputs.list.remove(position);
            log::info!(Audio, "source removed: {} ({})", input.name, input.description);
            self.publish();
        }
    }

    fn set_default(&mut self, default: Option<String>) {
        if self.inputs.default == default {
            return;
        }
        log::info!(
            Audio,
            "default source changed: {} -> {}",
            self.inputs.default.as_deref().unwrap_or("none"),
            default.as_deref().unwrap_or("none")
        );
        self.inputs.default = default;
        self.publish();
    }

    /// Description of a source by name, or the name itself when the source is
    /// not in the list.
    fn description_of(&self, name: &str) -> String {
        self.inputs
            .list
            .iter()
            .find(|input| input.name == name)
            .map_or_else(|| name.to_string(), |input| input.description.clone())
    }
}

/// None for monitors of outputs and for sources without a name.
fn input_from(info: &SourceInfo) -> Option<Input> {
    if info.monitor_of_sink.is_some() {
        return None;
    }
    let name = info.name.as_ref()?.to_string();
    let description = info
        .description
        .as_ref()
        .map_or_else(|| name.clone(), |description| description.to_string());
    let form_factor = info.proplist.get_str(properties::DEVICE_FORM_FACTOR);
    let bus = info.proplist.get_str(properties::DEVICE_BUS);
    // A built-in microphone says so, or sits on the PCI bus (a USB or
    // Bluetooth one reports that bus instead).
    let built_in = form_factor.as_deref() == Some("internal") || bus.as_deref() == Some("pci");
    Some(Input {
        index: info.index,
        name,
        description,
        form_factor,
        bus,
        built_in,
    })
}

fn describe(input: &Input) -> String {
    format!(
        "{} ({}), bus {}, form factor {}{}",
        input.name,
        input.description,
        input.bus.as_deref().unwrap_or("-"),
        input.form_factor.as_deref().unwrap_or("-"),
        if input.built_in { ", built-in" } else { "" }
    )
}

/// The source a recording is taken from.
struct Resolved {
    name: String,
    description: String,
}

/// One recording in progress.
struct Capture {
    tag: String,
    stream: Stream,
    source: Resolved,
    requested_at: Instant,
    /// Answered when the stream becomes ready, or fails before that.
    pending_start: Option<oneshot::Sender<Result<(), AudioError>>>,
    ready: bool,
    failed: bool,
    sink: Sink,
}

struct Engine {
    mainloop: Mainloop,
    context: Option<Context>,
    inventory: Rc<RefCell<Inventory>>,
    capture: Option<Capture>,
    level: watch::Sender<f32>,
    connection_lost_logged: bool,
}

impl Engine {
    /// Connects to the sound server and lists its sources. Blocks this thread
    /// until the server answers or refuses. True when connected.
    fn connect(&mut self) -> bool {
        let Some(mut proplist) = Proplist::new() else {
            log::error!(Audio, "could not create a libpulse property list");
            return false;
        };
        let _ = proplist.set_str(properties::APPLICATION_NAME, APP_NAME);
        let _ = proplist.set_str(properties::APPLICATION_ID, APP_ID);

        let Some(mut context) = Context::new_with_proplist(&self.mainloop, APP_NAME, &proplist) else {
            log::error!(Audio, "could not create a libpulse context (library too old?)");
            return false;
        };
        if let Err(error) = context.connect(None, ContextFlags::NOAUTOSPAWN, None) {
            log::error!(Audio, "cannot reach the sound server: {error}");
            return false;
        }

        loop {
            match context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => {
                    log::error!(Audio, "cannot reach the sound server: {}", context.errno());
                    return false;
                }
                _ => {}
            }
            if !self.iterate_once() {
                return false;
            }
        }

        // The first listing, before subscribing, so a source is not reported
        // as added while it is being listed.
        let introspector = context.introspect();
        self.inventory.borrow_mut().inputs = Inputs::default();
        let list_done = Rc::new(Cell::new(false));
        let server_done = Rc::new(Cell::new(false));
        {
            let inventory = Rc::clone(&self.inventory);
            let done = Rc::clone(&list_done);
            introspector.get_source_info_list(move |result| match result {
                ListResult::Item(info) => {
                    if let Some(input) = input_from(info) {
                        inventory.borrow_mut().inputs.list.push(input);
                    }
                }
                ListResult::End | ListResult::Error => done.set(true),
            });
        }
        {
            let inventory = Rc::clone(&self.inventory);
            let done = Rc::clone(&server_done);
            introspector.get_server_info(move |info: &ServerInfo| {
                log::info!(
                    Audio,
                    "sound server: {} {}",
                    info.server_name.as_deref().unwrap_or("?"),
                    info.server_version.as_deref().unwrap_or("?")
                );
                inventory.borrow_mut().inputs.default =
                    info.default_source_name.as_ref().map(|name| name.to_string());
                done.set(true);
            });
        }
        while !(list_done.get() && server_done.get()) {
            if !context.get_state().is_good() || !self.iterate_once() {
                log::error!(Audio, "the sound server went away while listing sources");
                return false;
            }
        }

        {
            let inventory = self.inventory.borrow();
            if inventory.inputs.list.is_empty() {
                log::warning!(Audio, "no microphones");
            }
            for input in &inventory.inputs.list {
                log::info!(Audio, "microphone: {}", describe(input));
            }
            log::info!(
                Audio,
                "default source: {}",
                inventory.inputs.default.as_deref().unwrap_or("none")
            );
            inventory.publish();
        }

        let inventory = Rc::clone(&self.inventory);
        let watcher = context.introspect();
        context.set_subscribe_callback(Some(Box::new(move |facility, change, index| {
            on_server_event(&watcher, &inventory, facility, change, index);
        })));
        context.subscribe(InterestMaskSet::SOURCE | InterestMaskSet::SERVER, |success| {
            if !success {
                log::warning!(Audio, "could not subscribe to source changes");
            }
        });

        self.context = Some(context);
        self.connection_lost_logged = false;
        true
    }

    /// One blocking pass of the main loop. False when the loop failed.
    fn iterate_once(&mut self) -> bool {
        match self.mainloop.iterate(true) {
            IterateResult::Success(_) => true,
            IterateResult::Quit(_) => false,
            IterateResult::Err(error) => {
                log::error!(Audio, "libpulse main loop failed: {error}");
                false
            }
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Start {
                microphone,
                tag,
                reply,
            } => self.start(&microphone, tag, reply),
            Command::Stop { reply } => {
                let _ = reply.send(self.stop());
            }
            Command::Cancel => self.cancel(),
            // Handled by the loop in run().
            Command::Quit => {}
        }
    }

    fn start(
        &mut self,
        microphone: &Microphone,
        tag: String,
        reply: oneshot::Sender<Result<(), AudioError>>,
    ) {
        if self.capture.is_some() {
            let _ = reply.send(Err(AudioError::CouldNotStart("already recording".to_string())));
            return;
        }
        let requested_at = Instant::now();

        // The server may have restarted since the last recording.
        let connected = self
            .context
            .as_ref()
            .is_some_and(|context| context.get_state() == ContextState::Ready);
        if !connected {
            self.drop_context();
            if !self.connect() {
                let _ = reply.send(Err(AudioError::CouldNotStart(
                    "the sound server is not reachable".to_string(),
                )));
                return;
            }
        }

        let source = match self.resolve(microphone, &tag) {
            Ok(source) => source,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };

        let sink = match Sink::create(&tag, requested_at) {
            Ok(sink) => sink,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };

        let Some(context) = self.context.as_mut() else {
            sink.discard();
            let _ = reply.send(Err(AudioError::CouldNotStart("no sound server".to_string())));
            return;
        };
        let Some(mut stream) = Stream::new(context, STREAM_NAME, &SAMPLE_SPEC, None) else {
            sink.discard();
            let _ = reply.send(Err(AudioError::CouldNotStart(format!(
                "could not create a stream: {}",
                context.errno()
            ))));
            return;
        };

        // u32::MAX leaves a field to the server. With ADJUST_LATENCY the
        // fragment size is what the server aims to deliver per read.
        let attributes = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: FRAGMENT_BYTES,
        };
        if let Err(error) =
            stream.connect_record(Some(&source.name), Some(&attributes), StreamFlags::ADJUST_LATENCY)
        {
            sink.discard();
            let _ = reply.send(Err(AudioError::CouldNotStart(format!(
                "could not connect to {}: {error}",
                source.name
            ))));
            return;
        }

        self.capture = Some(Capture {
            tag,
            stream,
            source,
            requested_at,
            pending_start: Some(reply),
            ready: false,
            failed: false,
            sink,
        });
    }

    /// Turns the setting into a source the server has right now.
    fn resolve(&self, microphone: &Microphone, tag: &str) -> Result<Resolved, AudioError> {
        let inventory = self.inventory.borrow();
        let list = &inventory.inputs.list;
        let by_name = |name: &str| Resolved {
            name: name.to_string(),
            description: inventory.description_of(name),
        };
        // The server's default, or the first source when it names none.
        let fallback = inventory
            .inputs
            .default
            .as_deref()
            .or_else(|| list.first().map(|input| input.name.as_str()))
            .map(by_name);

        match microphone {
            Microphone::Auto => fallback.ok_or(AudioError::NoMicrophone),
            // A machine without a built-in microphone records from the
            // default, and no one needs to be told, as on macOS.
            Microphone::BuiltIn => list
                .iter()
                .find(|input| input.built_in)
                .map(|input| by_name(input.name.as_str()))
                .or(fallback)
                .ok_or(AudioError::NoMicrophone),
            Microphone::Named(name) => {
                if list.iter().any(|input| &input.name == name) {
                    return Ok(by_name(name.as_str()));
                }
                let fallback = fallback.ok_or(AudioError::NoMicrophone)?;
                log::warning!(
                    Audio,
                    "[{tag}] chosen input {name} is not connected, using {}",
                    fallback.name
                );
                Ok(fallback)
            }
        }
    }

    /// After every pass of the main loop: notices the stream becoming ready
    /// or failing, and moves whatever audio has arrived into the file.
    fn service_capture(&mut self) {
        let Some(capture) = self.capture.as_mut() else { return };

        match capture.stream.get_state() {
            StreamState::Ready if !capture.ready => {
                capture.ready = true;
                let device = capture
                    .stream
                    .get_device_name()
                    .map_or_else(|| capture.source.name.clone(), |name| name.to_string());
                let format = capture
                    .stream
                    .get_sample_spec()
                    .map_or_else(|| "unknown format".to_string(), |spec| spec.print());
                let fragment = capture
                    .stream
                    .get_buffer_attr()
                    .map_or_else(|| "?".to_string(), |attributes| attributes.fragsize.to_string());
                log::info!(
                    Audio,
                    "[{}] capture started from {} ({}), {}, fragment {} bytes, ready after {} ms",
                    capture.tag,
                    capture.source.description,
                    device,
                    format,
                    fragment,
                    capture.requested_at.elapsed().as_millis()
                );
                if let Some(reply) = capture.pending_start.take() {
                    let _ = reply.send(Ok(()));
                }
            }
            StreamState::Failed | StreamState::Terminated if !capture.failed => {
                capture.failed = true;
                let reason = self
                    .context
                    .as_ref()
                    .map_or_else(|| "connection lost".to_string(), |context| format!("{}", context.errno()));
                if let Some(reply) = capture.pending_start.take() {
                    log::error!(
                        Audio,
                        "[{}] stream from {} failed to start: {reason}",
                        capture.tag,
                        capture.source.name
                    );
                    let _ = reply.send(Err(AudioError::CouldNotStart(reason)));
                    if let Some(capture) = self.capture.take() {
                        capture.sink.discard();
                    }
                    return;
                }
                // What was recorded so far stays; stop() finishes the file.
                log::error!(Audio, "[{}] stream ended during recording: {reason}", capture.tag);
                self.level.send_replace(0.0);
            }
            _ => {}
        }

        if capture.ready && !capture.failed {
            read_available(capture, &self.level);
        }
    }

    fn stop(&mut self) -> Result<Recording, AudioError> {
        let Some(mut capture) = self.capture.take() else {
            return Err(AudioError::CouldNotStart("nothing is recording".to_string()));
        };
        if capture.ready && !capture.failed {
            read_available(&mut capture, &self.level);
        }
        close(&mut capture);
        self.level.send_replace(0.0);
        capture.sink.finish()
    }

    fn cancel(&mut self) {
        let Some(mut capture) = self.capture.take() else { return };
        close(&mut capture);
        self.level.send_replace(0.0);
        log::info!(Audio, "[{}] recording cancelled, file removed", capture.tag);
        capture.sink.discard();
    }

    /// Notices the server going away while nothing is recording. The next
    /// recording connects again.
    fn check_context(&mut self) {
        let lost = self
            .context
            .as_ref()
            .is_some_and(|context| !context.get_state().is_good());
        if lost && !self.connection_lost_logged {
            self.connection_lost_logged = true;
            let reason = self
                .context
                .as_ref()
                .map_or_else(String::new, |context| format!("{}", context.errno()));
            log::warning!(Audio, "lost the sound server ({reason}), will reconnect on the next recording");
        }
    }

    /// Drops the context. The subscription callback holds a reference to the
    /// context through its introspector, so it is removed first.
    fn drop_context(&mut self) {
        if let Some(mut context) = self.context.take() {
            context.set_subscribe_callback(None);
            context.disconnect();
        }
    }

    fn shut_down(&mut self) {
        self.cancel();
        self.drop_context();
    }
}

/// Stops the stream for good. No callbacks are set on it, so dropping it
/// afterwards needs nothing else.
fn close(capture: &mut Capture) {
    if let Some(reply) = capture.pending_start.take() {
        let _ = reply.send(Err(AudioError::CouldNotStart(
            "stopped before the stream was ready".to_string(),
        )));
    }
    if capture.stream.get_state().is_good() {
        if let Err(error) = capture.stream.disconnect() {
            log::warning!(Audio, "[{}] disconnecting the stream failed: {error}", capture.tag);
        }
    }
}

/// Moves every fragment the stream holds into the sink.
fn read_available(capture: &mut Capture, level: &watch::Sender<f32>) {
    loop {
        match capture.stream.peek() {
            Ok(PeekResult::Empty) => break,
            Ok(PeekResult::Hole(bytes)) => {
                log::debug!(Audio, "[{}] hole of {bytes} bytes in the stream", capture.tag);
            }
            Ok(PeekResult::Data(bytes)) => {
                let smoothed = capture.sink.consume(bytes);
                level.send_replace(smoothed);
            }
            Err(error) => {
                log::error!(Audio, "[{}] reading the stream failed: {error}", capture.tag);
                break;
            }
        }
        if let Err(error) = capture.stream.discard() {
            log::error!(Audio, "[{}] dropping a fragment failed: {error}", capture.tag);
            break;
        }
    }
}

/// Subscription callback: keeps the inventory in step with the server.
fn on_server_event(
    introspector: &Introspector,
    inventory: &Rc<RefCell<Inventory>>,
    facility: Option<Facility>,
    change: Option<Change>,
    index: u32,
) {
    match (facility, change) {
        (Some(Facility::Source), Some(Change::New | Change::Changed)) => {
            let inventory = Rc::clone(inventory);
            introspector.get_source_info_by_index(index, move |result| {
                if let ListResult::Item(info) = result {
                    if let Some(input) = input_from(info) {
                        inventory.borrow_mut().upsert(input);
                    }
                }
            });
        }
        (Some(Facility::Source), Some(Change::Removed)) => {
            inventory.borrow_mut().remove(index);
        }
        (Some(Facility::Server), Some(Change::Changed)) => {
            let inventory = Rc::clone(inventory);
            introspector.get_server_info(move |info: &ServerInfo| {
                let default = info.default_source_name.as_ref().map(|name| name.to_string());
                inventory.borrow_mut().set_default(default);
            });
        }
        _ => {}
    }
}
