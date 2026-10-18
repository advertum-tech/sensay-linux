// Talks to the speech service over HTTP, as HTTPTranscriptionService.swift.
// The service is OpenAI-compatible: local, or remote at an https address with
// an optional key.
//
// There is no queue here. The order of requests is kept in recogniser.rs, so
// that a recording still waiting its turn goes to whatever service is chosen
// when its turn comes rather than the one chosen when it was recorded.

pub mod recogniser;

use std::error::Error as _;
use std::io;
use std::path::Path;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use reqwest::multipart::{Form, Part};
use reqwest::{Body, Client, StatusCode};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::config::SpeechService;
use crate::log;

/// How long one request may take before it counts as unanswered: half a
/// minute plus twice the length of the recording (RecognitionTiming.swift).
pub fn timeout(duration: f64) -> Duration {
    Duration::from_secs_f64(30.0 + 2.0 * duration.max(0.0))
}

/// What the request is made with. Read when a row's turn comes.
#[derive(Clone)]
pub struct ServiceSettings {
    pub service: SpeechService,
    /// Empty means the request goes without an authorization header.
    pub api_key: String,
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
            TranscriptionError::Unauthorized => "The speech service refused the API key.".to_string(),
            TranscriptionError::Restarting => {
                "The speech service is restarting. Try again in a moment.".to_string()
            }
            TranscriptionError::TimedOut(seconds) => {
                format!("The speech service did not answer in {seconds} s.")
            }
            TranscriptionError::Failed(status) => {
                format!("The speech service answered with code {status}.")
            }
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
    let base = settings.service.url();
    let Some(client) = client else {
        return Err(TranscriptionError::ServiceDown(base.to_string()));
    };
    let endpoint = format!("{base}/v1/audio/transcriptions");

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
    let form = Form::new()
        .part("file", file)
        .text("response_format", "verbose_json");

    let mut request = client.post(&endpoint).multipart(form);
    if !settings.api_key.is_empty() {
        request = request
            .header("X-Api-Key", &settings.api_key)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", settings.api_key));
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
            check(status)?;
            parse(&body)
        }
    }
}

/// Maps the status code to the cases the user is shown.
fn check(status: StatusCode) -> Result<(), TranscriptionError> {
    match status.as_u16() {
        200..=299 => Ok(()),
        400 => Err(TranscriptionError::BadAudio),
        401 | 403 => Err(TranscriptionError::Unauthorized),
        503 => Err(TranscriptionError::Restarting),
        other => Err(TranscriptionError::Failed(other)),
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
        assert_eq!(check(StatusCode::OK), Ok(()));
        assert_eq!(check(StatusCode::BAD_REQUEST), Err(TranscriptionError::BadAudio));
        assert_eq!(check(StatusCode::UNAUTHORIZED), Err(TranscriptionError::Unauthorized));
        assert_eq!(check(StatusCode::FORBIDDEN), Err(TranscriptionError::Unauthorized));
        assert_eq!(check(StatusCode::SERVICE_UNAVAILABLE), Err(TranscriptionError::Restarting));
        assert_eq!(check(StatusCode::BAD_GATEWAY), Err(TranscriptionError::Failed(502)));
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
