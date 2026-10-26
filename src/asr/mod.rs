// Talks to the speech service over HTTP, as HTTPTranscriptionService.swift.
// The service is OpenAI-compatible: the Sensay API, which picks the model by
// the `model` field and learns from two headers which computer and which
// build is asking, or a local server, which gets neither.
//
// There is no queue here. The order of requests is kept in recogniser.rs, so
// that a recording still waiting its turn goes to whatever service is chosen
// when its turn comes rather than the one chosen when it was recorded.

pub mod recogniser;

use std::error::Error as _;
use std::io;
use std::path::Path;
use std::sync::LazyLock;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use reqwest::multipart::{Form, Part};
use reqwest::{Body, Client, StatusCode};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::app_id;
use crate::config::SpeechService;
use crate::log;

/// The Sensay API: where Sensay Whisper and Sensay Parakeet are sent. Built
/// in, not a setting.
const SENSAY_API: &str = "https://api.sensay.app";

/// Shown for a ban when the API sends no text of its own.
const BANNED: &str =
    "Sensay is blocked on this computer. If you think this is a mistake, contact us at hello@advertum.com";
/// Shown for a used-up limit when the API sends no text of its own, and kept
/// in the history for every used-up limit.
const LIMIT_REACHED: &str = "Daily limit reached.";

/// "Sensay/0.2.1 (Ubuntu 22.04; x86_64)" for the Sensay API: the build, the
/// system and the processor. Worked out once.
static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let agent = format!(
        "Sensay/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        system_name(),
        std::env::consts::ARCH
    );
    // A header carries printable ASCII only.
    agent.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').collect()
});

/// Read once: it cannot change while the app runs. None without a machine
/// id; the request then goes without the header.
static APP_ID: LazyLock<Option<String>> = LazyLock::new(app_id::app_id);

/// How long one request may take before it counts as unanswered: half a
/// minute plus twice the length of the recording (RecognitionTiming.swift).
pub fn timeout(duration: f64) -> Duration {
    Duration::from_secs_f64(30.0 + 2.0 * duration.max(0.0))
}

/// What the request is made with. Read when a row's turn comes.
#[derive(Clone)]
pub struct ServiceSettings {
    pub service: SpeechService,
    /// Where Local is sent.
    pub local_address: String,
}

impl ServiceSettings {
    /// The base address; the request goes to `/v1/audio/transcriptions`
    /// under it.
    fn address(&self) -> &str {
        match self.service {
            SpeechService::Whisper | SpeechService::Parakeet => SENSAY_API,
            SpeechService::Local => &self.local_address,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Transcription {
    /// Paragraphs are already separated by the service and are taken as
    /// they come.
    pub text: String,
    pub language: Option<String>,
}

/// The cases the user is told about, named after the error table in the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionError {
    /// No connection to the service at this address.
    ServiceDown(String),
    /// HTTP 400, or the recording could not be read.
    BadAudio,
    /// HTTP 401 or 403.
    Unauthorized,
    /// HTTP 503.
    Restarting,
    /// No answer within the time the recording was given, in seconds.
    TimedOut(u64),
    /// Any other status.
    Failed(u16),
    /// HTTP 403 or 429 from the Sensay API with its reason. The text is the
    /// API's own, shown as it is; empty when the API sent none.
    Refused(Refusal, String),
}

/// Why the Sensay API refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The computer is blocked.
    Ban,
    /// Its daily limit is used up.
    Limit,
}

impl TranscriptionError {
    /// The wording of ErrorPresenter.swift.
    pub fn message(&self) -> String {
        match self {
            TranscriptionError::ServiceDown(address) => {
                format!("The speech service is not running at {address}.")
            }
            TranscriptionError::BadAudio => {
                "The recording was not accepted by the speech service.".to_string()
            }
            TranscriptionError::Unauthorized => "The speech service refused the request.".to_string(),
            TranscriptionError::Restarting => {
                "The speech service is restarting. Try again in a moment.".to_string()
            }
            TranscriptionError::TimedOut(seconds) => {
                format!("The speech service did not answer in {seconds} s.")
            }
            TranscriptionError::Failed(status) => {
                format!("The speech service answered with code {status}.")
            }
            // The API's own text comes first: it can change without a new
            // build of the app.
            TranscriptionError::Refused(_, text) if !text.trim().is_empty() => text.clone(),
            TranscriptionError::Refused(Refusal::Ban, _) => BANNED.to_string(),
            TranscriptionError::Refused(Refusal::Limit, _) => LIMIT_REACHED.to_string(),
        }
    }

    /// What the history keeps: the message, except for a used-up limit. The
    /// API's text for it counts down to the reset, which read an hour later
    /// is wrong.
    pub fn history_text(&self) -> String {
        match self {
            TranscriptionError::Refused(Refusal::Limit, _) => LIMIT_REACHED.to_string(),
            other => other.message(),
        }
    }

    /// Ban or limit when the Sensay API refused the dictation.
    pub fn refusal(&self) -> Option<Refusal> {
        match self {
            TranscriptionError::Refused(refusal, _) => Some(*refusal),
            _ => None,
        }
    }
}

/// One client for every request. None when it could not be built; every
/// request then fails as if the service were down, with the reason logged.
pub fn client() -> Option<Client> {
    match Client::builder()
        .user_agent(concat!("Sensay/", env!("CARGO_PKG_VERSION"), " (Linux)"))
        .build()
    {
        Ok(client) => Some(client),
        Err(error) => {
            log::error!(Asr, "HTTP client could not start: {}", chain(&error));
            None
        }
    }
}

/// Sends one recording and returns its text. `sent` fires once, when the last
/// byte of the audio has been handed to the connection: until then the row
/// says it is sending, after it that it is being transcribed.
pub async fn transcribe(
    client: Option<&Client>,
    settings: &ServiceSettings,
    audio: &Path,
    timeout: Duration,
    sent: oneshot::Sender<()>,
) -> Result<Transcription, TranscriptionError> {
    let base = settings.address();
    let Some(client) = client else {
        return Err(TranscriptionError::ServiceDown(base.to_string()));
    };
    let endpoint = format!("{}/v1/audio/transcriptions", base.trim_end_matches('/'));

    let audio = match tokio::fs::read(audio).await {
        Ok(audio) => Bytes::from(audio),
        Err(error) => {
            log::error!(Asr, "cannot read {}: {error}", audio.display());
            return Err(TranscriptionError::BadAudio);
        }
    };
    let length = audio.len() as u64;
    let file = Part::stream_with_length(notifying_body(audio, sent), length)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|_| TranscriptionError::BadAudio)?;
    let mut form = Form::new()
        .part("file", file)
        .text("response_format", "verbose_json");
    let model = settings.service.model();
    if let Some(model) = model {
        form = form.text("model", model);
    }

    let mut request = client.post(&endpoint).multipart(form);
    // The Sensay API learns which computer and which build is asking. A
    // local server gets the request as before.
    if model.is_some() {
        request = request.header(reqwest::header::USER_AGENT, USER_AGENT.as_str());
        if let Some(app_id) = APP_ID.as_deref() {
            request = request.header("X-App-ID", app_id);
        }
    }

    // The whole request against one clock, the answer included: a service
    // that keeps the connection open while saying nothing is caught too.
    let call = async {
        let response = request.send().await?;
        let status = response.status();
        let body = response.bytes().await?;
        Ok::<_, reqwest::Error>((status, body))
    };
    let seconds = timeout.as_secs_f64().round() as u64;
    match tokio::time::timeout(timeout, call).await {
        Err(_) => {
            log::error!(Asr, "request to {endpoint} timed out after {seconds} s");
            Err(TranscriptionError::TimedOut(seconds))
        }
        // The user is told only that the service could not be reached; which
        // way it could not be reached is in the log.
        Ok(Err(error)) => {
            log::error!(Asr, "request to {endpoint} failed: {}", chain(&error));
            Err(TranscriptionError::ServiceDown(base.to_string()))
        }
        Ok(Ok((status, body))) => {
            check(status, &body)?;
            parse(&body)
        }
    }
}

/// Maps the status code to the cases the user is shown. A 403 or 429 with
/// the Sensay API's reason in it is a refusal; without one, it is mapped as
/// before.
fn check(status: StatusCode, body: &[u8]) -> Result<(), TranscriptionError> {
    if matches!(status.as_u16(), 403 | 429) {
        if let Some(refused) = refusal(body) {
            return Err(refused);
        }
    }
    match status.as_u16() {
        200..=299 => Ok(()),
        400 => Err(TranscriptionError::BadAudio),
        401 | 403 => Err(TranscriptionError::Unauthorized),
        503 => Err(TranscriptionError::Restarting),
        other => Err(TranscriptionError::Failed(other)),
    }
}

/// The Sensay API's reason, in the OpenAI error format:
/// `{"error": {"type": "ban" or "limit", "message": "..."}}`.
fn refusal(body: &[u8]) -> Option<TranscriptionError> {
    #[derive(Deserialize)]
    struct Payload {
        error: Detail,
    }
    #[derive(Deserialize)]
    struct Detail {
        r#type: Option<String>,
        message: Option<String>,
    }
    let payload: Payload = serde_json::from_slice(body).ok()?;
    let refusal = match payload.error.r#type.as_deref()? {
        "ban" => Refusal::Ban,
        "limit" => Refusal::Limit,
        _ => return None,
    };
    Some(TranscriptionError::Refused(refusal, payload.error.message.unwrap_or_default()))
}

/// The distribution and its version from os-release(5): "Ubuntu 22.04", or
/// the name alone on a distribution without versions, as Arch.
fn system_name() -> String {
    // /usr/lib/os-release is read where /etc has none.
    let text = ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    os_release_name(&text)
}

fn os_release_name(text: &str) -> String {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let value = line.trim().strip_prefix(key)?.strip_prefix('=')?;
            Some(value.trim_matches(['"', '\'']).to_string()).filter(|value| !value.is_empty())
        })
    };
    let name = value("NAME").unwrap_or_else(|| "Linux".to_string());
    match value("VERSION_ID") {
        Some(version) => format!("{name} {version}"),
        None => name,
    }
}

/// Only the fields the app uses.
fn parse(body: &[u8]) -> Result<Transcription, TranscriptionError> {
    #[derive(Deserialize)]
    struct Payload {
        text: String,
        language: Option<String>,
    }
    let payload: Payload = serde_json::from_slice(body).map_err(|error| {
        log::error!(Asr, "answer is not the expected JSON: {error}");
        TranscriptionError::Failed(200)
    })?;
    Ok(Transcription {
        text: payload.text,
        language: payload.language,
    })
}

/// The audio as a stream of chunks that says when its last chunk has been
/// taken by the connection.
fn notifying_body(audio: Bytes, sent: oneshot::Sender<()>) -> Body {
    const CHUNK: usize = 64 * 1024;
    let mut offset = 0;
    let mut sent = Some(sent);
    let chunks = futures_util::stream::poll_fn(move |_| {
        if offset < audio.len() {
            let end = (offset + CHUNK).min(audio.len());
            let chunk = audio.slice(offset..end);
            offset = end;
            Poll::Ready(Some(Ok::<Bytes, io::Error>(chunk)))
        } else {
            if let Some(sent) = sent.take() {
                let _ = sent.send(());
            }
            Poll::Ready(None)
        }
    });
    Body::wrap_stream(chunks)
}

/// The error and every cause under it, for the log.
fn chain(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_map_to_the_cases_the_user_is_shown() {
        assert_eq!(check(StatusCode::OK, b""), Ok(()));
        assert_eq!(check(StatusCode::BAD_REQUEST, b""), Err(TranscriptionError::BadAudio));
        assert_eq!(check(StatusCode::UNAUTHORIZED, b""), Err(TranscriptionError::Unauthorized));
        assert_eq!(check(StatusCode::FORBIDDEN, b""), Err(TranscriptionError::Unauthorized));
        assert_eq!(check(StatusCode::TOO_MANY_REQUESTS, b"{}"), Err(TranscriptionError::Failed(429)));
        assert_eq!(check(StatusCode::SERVICE_UNAVAILABLE, b""), Err(TranscriptionError::Restarting));
        assert_eq!(check(StatusCode::BAD_GATEWAY, b""), Err(TranscriptionError::Failed(502)));
    }

    #[test]
    fn a_refusal_from_the_sensay_api_is_told_apart() {
        let ban = br#"{"error":{"type":"ban","message":"Blocked."}}"#;
        let limit = br#"{"error":{"type":"limit","message":""}}"#;
        assert_eq!(
            check(StatusCode::FORBIDDEN, ban),
            Err(TranscriptionError::Refused(Refusal::Ban, "Blocked.".to_string()))
        );
        assert_eq!(
            check(StatusCode::TOO_MANY_REQUESTS, limit),
            Err(TranscriptionError::Refused(Refusal::Limit, String::new()))
        );
    }

    #[test]
    fn a_refusal_shows_the_api_text_and_the_history_a_short_one_for_the_limit() {
        let text = "Daily limit of 10,000 words reached. It resets in 3 h.";
        let limit = TranscriptionError::Refused(Refusal::Limit, text.to_string());
        assert_eq!(limit.message(), text);
        assert_eq!(limit.history_text(), LIMIT_REACHED);
        assert_eq!(TranscriptionError::Refused(Refusal::Limit, String::new()).message(), LIMIT_REACHED);
        let ban = TranscriptionError::Refused(Refusal::Ban, String::new());
        assert_eq!(ban.message(), BANNED);
        assert_eq!(ban.history_text(), BANNED);
    }

    #[test]
    fn the_system_is_named_from_os_release() {
        let ubuntu = "PRETTY_NAME=\"Ubuntu 22.04.5 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"22.04\"\n";
        assert_eq!(os_release_name(ubuntu), "Ubuntu 22.04");
        assert_eq!(os_release_name("NAME=\"Arch Linux\"\nBUILD_ID=rolling\n"), "Arch Linux");
        assert_eq!(os_release_name(""), "Linux");
    }

    #[test]
    fn answer_text_and_language_are_read() {
        let answer = br#"{"text":"Hello there.","language":"en","duration":1.5,"segments":[]}"#;
        let transcription = parse(answer).unwrap();
        assert_eq!(transcription.text, "Hello there.");
        assert_eq!(transcription.language.as_deref(), Some("en"));
        assert_eq!(parse(b"not json").unwrap_err(), TranscriptionError::Failed(200));
    }

    #[test]
    fn timeout_grows_with_the_recording() {
        assert_eq!(timeout(5.0), Duration::from_secs(40));
        assert_eq!(timeout(60.0), Duration::from_secs(150));
    }
}
