use std::collections::HashMap;
use std::sync::mpsc::Sender as StdSender;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::audio;

const REALTIME_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";
const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const LOCAL_SPEECH_SIGNAL_INTERVAL: Duration = Duration::from_millis(250);
const LOCAL_SPEECH_RMS_THRESHOLD: f64 = 0.015;
const LOCAL_SPEECH_PEAK_THRESHOLD: f64 = 0.08;

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub api_key: String,
    pub terms: Vec<String>,
    pub transcription_prompt: String,
    pub vad_threshold: f32,
    pub vad_silence_ms: u32,
}

#[derive(Debug)]
pub enum SessionCommand {
    Finish,
    Stop,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Status(String),
    Preview(String),
    LocalSpeechDetected,
    AppendCommitted(String),
    CorrectionTimerElapsed(u64),
    CorrectionReady(CorrectionResult),
    CorrectionFailed(u64),
    TranslationReady(TranslationTarget, String),
    TranslationStarted(TranslationTarget),
    RecordingStarted(mpsc::UnboundedSender<SessionCommand>),
    RecordingFinished,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationTarget {
    Japanese,
    English,
}

#[derive(Debug, Clone)]
pub struct CorrectionResult {
    pub generation: u64,
    pub original: String,
    pub snapshot: String,
    pub corrected: String,
}

pub async fn start_transcription(
    options: SessionOptions,
    ui_tx: StdSender<UiEvent>,
) -> Result<mpsc::UnboundedSender<SessionCommand>> {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let task_ui_tx = ui_tx.clone();
    tokio::spawn(async move {
        let result = run_transcription(options, task_ui_tx.clone(), command_rx).await;
        if let Err(error) = result {
            tracing::error!("transcription session failed: {error:#}");
            let _ = task_ui_tx.send(UiEvent::Error(error.to_string()));
            let _ = task_ui_tx.send(UiEvent::RecordingFinished);
        }
    });
    Ok(command_tx)
}

async fn run_transcription(
    options: SessionOptions,
    ui_tx: StdSender<UiEvent>,
    mut command_rx: mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<()> {
    let mut request = REALTIME_URL
        .into_client_request()
        .context("invalid realtime URL")?;
    request.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {}", options.api_key).parse()?,
    );
    request
        .headers_mut()
        .insert("OpenAI-Beta", "realtime=v1".parse()?);

    let (ws_stream, _) = connect_async(request)
        .await
        .context("failed to connect to OpenAI Realtime API")?;
    let (mut write, mut read) = ws_stream.split();

    let prompt = build_transcription_prompt(&options.transcription_prompt, &options.terms);
    let threshold = normalize_vad_threshold(options.vad_threshold);
    let session_update = json!({
        "type": "transcription_session.update",
        "session": {
            "input_audio_format": "pcm16",
            "input_audio_noise_reduction": { "type": "near_field" },
            "input_audio_transcription": {
                "model": "gpt-4o-transcribe",
                "language": "ja",
                "prompt": prompt
            },
            "turn_detection": {
                "type": "server_vad",
                "threshold": threshold,
                "prefix_padding_ms": 300,
                "silence_duration_ms": options.vad_silence_ms
            },
            "include": ["item.input_audio_transcription.logprobs"]
        }
    });
    write
        .send(Message::Text(session_update.to_string().into()))
        .await
        .context("failed to send transcription_session.update")?;

    let (audio_tx, mut audio_rx) = mpsc::unbounded_channel();
    let capture = audio::start_capture(audio_tx).await?;
    let mut capture = Some(capture);
    let _ = ui_tx.send(UiEvent::Status("録音中".to_string()));
    let mut assembler = TranscriptAssembler::default();
    let mut pending_audio_bytes = 0_usize;
    let mut last_local_speech_signal = None;

    let mut finish_deadline: Option<Instant> = None;

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                match command {
                    SessionCommand::Finish => {
                        if finish_deadline.is_none() {
                            if let Some(capture) = capture.take() {
                                capture.stop().await;
                            }
                            tracing::info!(
                                "finish requested; stopping capture with {pending_audio_bytes} locally buffered bytes and waiting for server_vad to settle"
                            );
                            let settle_ms = (options.vad_silence_ms + 400).max(1_200) as u64;
                            finish_deadline = Some(Instant::now() + Duration::from_millis(settle_ms));
                            let _ = ui_tx.send(UiEvent::Status("最終結果を待っています".to_string()));
                        }
                    }
                    SessionCommand::Stop => {
                        if let Some(capture) = capture.take() {
                            capture.stop().await;
                        }
                        break;
                    }
                }
            }
            Some(chunk) = audio_rx.recv(), if finish_deadline.is_none() => {
                pending_audio_bytes += chunk.len();
                if chunk_has_local_speech(&chunk)
                    && should_emit_local_speech_signal(&mut last_local_speech_signal)
                {
                    let _ = ui_tx.send(UiEvent::LocalSpeechDetected);
                }
                let append = json!({
                    "type": "input_audio_buffer.append",
                    "audio": base64::engine::general_purpose::STANDARD.encode(chunk),
                });
                write
                    .send(Message::Text(append.to_string().into()))
                    .await
                    .context("failed to send input_audio_buffer.append")?;
            }
            message = read.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        if event_type(&text).as_deref() == Some("input_audio_buffer.committed") {
                            pending_audio_bytes = 0;
                        }
                        handle_realtime_event(&text, &mut assembler, &ui_tx);
                    }
                    Some(Ok(Message::Close(_))) => {
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::error!("realtime websocket read failed: {error}");
                        return Err(error).context("realtime websocket read failed");
                    }
                    None => break,
                }
            }
            _ = async {
                if let Some(deadline) = finish_deadline {
                    let now = Instant::now();
                    if deadline > now {
                        sleep(deadline - now).await;
                    }
                }
            }, if finish_deadline.is_some() => {
                break;
            }
        }
    }

    if let Some(capture) = capture.take() {
        capture.stop().await;
    }

    if let Some(provisional) = assembler.finish_provisional() {
        let _ = ui_tx.send(UiEvent::AppendCommitted(provisional));
    }
    let _ = ui_tx.send(UiEvent::Preview(String::new()));
    let _ = ui_tx.send(UiEvent::Status("待機中".to_string()));
    let _ = ui_tx.send(UiEvent::RecordingFinished);
    Ok(())
}

fn handle_realtime_event(
    text: &str,
    assembler: &mut TranscriptAssembler,
    ui_tx: &StdSender<UiEvent>,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        tracing::warn!("failed to parse realtime payload: {text}");
        return;
    };

    let Some(event_type) = value.get("type").and_then(Value::as_str) else {
        return;
    };

    match event_type {
        "input_audio_buffer.committed" => {
            let item_id = value
                .get("item_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let previous_item_id = value
                .get("previous_item_id")
                .and_then(Value::as_str)
                .map(str::to_string);

            if let Some(item_id) = item_id {
                for text in assembler.on_commit(item_id, previous_item_id) {
                    let _ = ui_tx.send(UiEvent::AppendCommitted(text));
                }
            }
        }
        "conversation.item.input_audio_transcription.delta" => {
            let item_id = value
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let preview = assembler.on_delta(item_id, delta);
            let _ = ui_tx.send(UiEvent::Preview(preview));
        }
        "conversation.item.input_audio_transcription.completed" => {
            let item_id = value
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let transcript = value
                .get("transcript")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            for text in assembler.on_completed(item_id, transcript) {
                let _ = ui_tx.send(UiEvent::AppendCommitted(text));
            }
            let _ = ui_tx.send(UiEvent::Preview(assembler.preview_text()));
        }
        "error" => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("OpenAI API returned an error")
                .to_string();
            tracing::error!("realtime API error: {message}; payload={value}");
            let _ = ui_tx.send(UiEvent::Error(message));
        }
        _ => {}
    }
}

fn event_type(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("type")?
        .as_str()
        .map(str::to_string)
}

fn should_emit_local_speech_signal(last_signal: &mut Option<Instant>) -> bool {
    let now = Instant::now();
    if last_signal.is_some_and(|instant| now.duration_since(instant) < LOCAL_SPEECH_SIGNAL_INTERVAL)
    {
        return false;
    }

    *last_signal = Some(now);
    true
}

fn chunk_has_local_speech(chunk: &[u8]) -> bool {
    let mut sum_squares = 0.0_f64;
    let mut sample_count = 0_u32;
    let mut peak = 0.0_f64;

    for sample in chunk.chunks_exact(2) {
        let value = i16::from_le_bytes([sample[0], sample[1]]) as f64 / i16::MAX as f64;
        let abs = value.abs();
        peak = peak.max(abs);
        sum_squares += value * value;
        sample_count += 1;
    }

    if sample_count == 0 {
        return false;
    }

    let rms = (sum_squares / sample_count as f64).sqrt();
    rms >= LOCAL_SPEECH_RMS_THRESHOLD || peak >= LOCAL_SPEECH_PEAK_THRESHOLD
}

pub async fn translate_text(api_key: &str, text: &str, instructions: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let response = client
        .post(RESPONSES_URL)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": "gpt-4.1-nano",
            "instructions": instructions,
            "input": text,
        }))
        .send()
        .await
        .context("failed to call Responses API")?
        .error_for_status()
        .context("Responses API returned an error")?;

    let value: Value = response
        .json()
        .await
        .context("failed to parse Responses API JSON")?;

    if let Some(output_text) = value.get("output_text").and_then(Value::as_str) {
        return Ok(output_text.trim().to_string());
    }

    extract_output_text(&value).context("response did not contain translated text")
}

pub async fn correct_transcript_text(
    api_key: &str,
    terms: &[String],
    text: &str,
) -> Result<String> {
    let client = reqwest::Client::new();
    let system_prompt = build_correction_prompt(terms);
    let response = client
        .post(RESPONSES_URL)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": "gpt-5.4-nano",
            "instructions": system_prompt,
            "input": text,
        }))
        .send()
        .await
        .context("failed to call Responses API for transcript correction")?
        .error_for_status()
        .context("Responses API returned an error for transcript correction")?;

    let value: Value = response
        .json()
        .await
        .context("failed to parse transcript correction JSON")?;

    if let Some(output_text) = value.get("output_text").and_then(Value::as_str) {
        return Ok(output_text.trim().to_string());
    }

    extract_output_text(&value).context("response did not contain corrected transcript text")
}

fn extract_output_text(value: &Value) -> Option<String> {
    let outputs = value.get("output")?.as_array()?;
    let mut fragments = Vec::new();

    for item in outputs {
        let contents = item.get("content")?.as_array()?;
        for content in contents {
            if content.get("type").and_then(Value::as_str) == Some("output_text") {
                if let Some(text) = content.get("text").and_then(Value::as_str) {
                    fragments.push(text.trim().to_string());
                }
            }
        }
    }

    let joined = fragments.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

pub fn build_transcription_prompt(base_prompt: &str, terms: &[String]) -> String {
    let mut prompt = base_prompt.trim().to_string();
    if prompt.is_empty() {
        prompt = "日本語の音声を自然な表記で正確に文字起こししてください。".to_string();
    }

    if terms.is_empty() {
        return prompt;
    }

    if !prompt.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("以下の語を優先して正確に認識してください:\n");
    for term in terms {
        prompt.push_str("- ");
        prompt.push_str(term);
        prompt.push('\n');
    }
    prompt
}

pub fn build_correction_prompt(terms: &[String]) -> String {
    let mut prompt = String::from(
        "You correct Japanese speech transcripts. Fix misspellings, punctuation, spacing, and capitalization only when needed. Preserve meaning. Do not summarize. Do not add facts. Return only the corrected transcript text.",
    );

    if !terms.is_empty() {
        prompt.push_str("\nPrefer these spellings exactly:\n");
        for term in terms {
            prompt.push_str("- ");
            prompt.push_str(term);
            prompt.push('\n');
        }
    }

    prompt
}

fn normalize_vad_threshold(value: f32) -> f64 {
    let clamped = value.clamp(0.0, 1.0) as f64;
    (clamped * 1_000_000_000_000_000_f64).round() / 1_000_000_000_000_000_f64
}

#[derive(Debug, Default)]
struct TranscriptAssembler {
    turns: HashMap<String, Turn>,
    last_appended_item_id: Option<String>,
}

#[derive(Debug, Default)]
struct Turn {
    committed: bool,
    previous_item_id: Option<String>,
    partial: String,
    completed: Option<String>,
    appended: bool,
}

impl TranscriptAssembler {
    fn on_commit(&mut self, item_id: String, previous_item_id: Option<String>) -> Vec<String> {
        let turn = self.turns.entry(item_id).or_default();
        turn.committed = true;
        turn.previous_item_id = previous_item_id;
        self.drain_ready()
    }

    fn on_delta(&mut self, item_id: String, delta: &str) -> String {
        let turn = self.turns.entry(item_id).or_default();
        turn.partial.push_str(delta);
        self.preview_text()
    }

    fn on_completed(&mut self, item_id: String, transcript: String) -> Vec<String> {
        let turn = self.turns.entry(item_id).or_default();
        turn.completed = Some(transcript);
        self.drain_ready()
    }

    fn preview_text(&self) -> String {
        self.turns
            .values()
            .filter(|turn| !turn.appended)
            .filter_map(|turn| {
                if let Some(completed) = &turn.completed {
                    if completed.is_empty() {
                        None
                    } else {
                        Some(completed.clone())
                    }
                } else if turn.partial.is_empty() {
                    None
                } else {
                    Some(turn.partial.clone())
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn finish_provisional(&mut self) -> Option<String> {
        let text = self
            .turns
            .values_mut()
            .filter(|turn| !turn.appended)
            .filter_map(|turn| {
                if let Some(completed) = &turn.completed {
                    turn.appended = true;
                    Some(completed.clone())
                } else if !turn.partial.trim().is_empty() {
                    turn.appended = true;
                    Some(turn.partial.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn drain_ready(&mut self) -> Vec<String> {
        let mut ready_texts = Vec::new();

        loop {
            let next_id = self.turns.iter().find_map(|(item_id, turn)| {
                if turn.appended || turn.completed.is_none() || !turn.committed {
                    return None;
                }

                let is_ready = match (&turn.previous_item_id, &self.last_appended_item_id) {
                    (None, None) => true,
                    (Some(previous), Some(last)) if previous == last => true,
                    _ => false,
                };

                if is_ready {
                    Some(item_id.clone())
                } else {
                    None
                }
            });

            let Some(next_id) = next_id else {
                break;
            };

            if let Some(turn) = self.turns.get_mut(&next_id) {
                turn.appended = true;
                self.last_appended_item_id = Some(next_id.clone());
                if let Some(text) = &turn.completed {
                    if !text.trim().is_empty() {
                        ready_texts.push(text.clone());
                    }
                }
            }
        }

        ready_texts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_terms() {
        let prompt = build_transcription_prompt(
            "日本語の音声を自然な表記で正確に文字起こししてください。",
            &["OpenAI".to_string(), "Ubuntu".to_string()],
        );
        assert!(prompt.contains("OpenAI"));
        assert!(prompt.contains("Ubuntu"));
    }

    #[test]
    fn correction_prompt_includes_terms() {
        let prompt = build_correction_prompt(&["OpenAI".to_string(), "Q.U.A.R.T.Z.".to_string()]);
        assert!(prompt.contains("OpenAI"));
        assert!(prompt.contains("Q.U.A.R.T.Z."));
    }

    #[test]
    fn assembler_waits_for_previous_item_id() {
        let mut assembler = TranscriptAssembler::default();
        assembler.on_commit("item-1".to_string(), None);
        assert_eq!(
            assembler.on_completed("item-2".to_string(), "second".to_string()),
            Vec::<String>::new()
        );
        assembler.on_commit("item-2".to_string(), Some("item-1".to_string()));
        assert_eq!(
            assembler.on_completed("item-1".to_string(), "first".to_string()),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn vad_threshold_serialization_stays_within_limit() {
        let value = normalize_vad_threshold(0.45000002_f32);
        let rendered = serde_json::to_string(&json!({ "threshold": value })).unwrap();
        let decimals = rendered
            .split(':')
            .nth(1)
            .unwrap()
            .trim_end_matches('}')
            .split('.')
            .nth(1)
            .unwrap();
        assert!(decimals.len() <= 16);
    }

    #[test]
    fn extracts_event_type() {
        assert_eq!(
            event_type(r#"{"type":"input_audio_buffer.committed","item_id":"x"}"#),
            Some("input_audio_buffer.committed".to_string())
        );
    }

    #[test]
    fn local_speech_detector_ignores_silence() {
        assert!(!chunk_has_local_speech(&[0; 960]));
    }

    #[test]
    fn local_speech_detector_finds_voice_like_energy() {
        let mut chunk = Vec::with_capacity(960);
        for _ in 0..480 {
            chunk.extend_from_slice(&1200_i16.to_le_bytes());
        }
        assert!(chunk_has_local_speech(&chunk));
    }
}
