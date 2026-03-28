use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use directories::UserDirs;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};

use crate::auth::AuthService;
use crate::config::{GeminiSttConfig, TranscriptionConfig};
use crate::providers::traits::TransientAudioInput;

/// Maximum upload size accepted by the shared STT subsystem (25 MB).
const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
/// Gemini inline request-size limit from the public audio docs (20 MB).
const GEMINI_INLINE_REQUEST_MAX_BYTES: usize = 20 * 1024 * 1024;
/// Request timeout for transcription API calls (seconds).
const TRANSCRIPTION_TIMEOUT_SECS: u64 = 120;
const GEMINI_PUBLIC_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const GEMINI_PUBLIC_UPLOAD_BASE: &str = "https://generativelanguage.googleapis.com/upload/v1beta";
const GEMINI_INTERNAL_API_BASE: &str = "https://cloudcode-pa.googleapis.com/v1internal";
const GEMINI_LOAD_CODE_ASSIST_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";

#[derive(Clone)]
pub struct TranscriptionRuntime {
    pub config: TranscriptionConfig,
    pub auth_service: Option<Arc<AuthService>>,
}

impl std::fmt::Debug for TranscriptionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptionRuntime")
            .field("enabled", &self.config.enabled)
            .field("default_provider", &self.config.default_provider)
            .field("max_duration_secs", &self.config.max_duration_secs)
            .field("has_auth_service", &self.auth_service.is_some())
            .finish()
    }
}

impl TranscriptionRuntime {
    pub fn from_config(config: TranscriptionConfig) -> Self {
        Self {
            config,
            auth_service: None,
        }
    }
}

#[derive(Clone)]
enum GeminiSttAuth {
    ApiKey(String),
    ManagedOAuth { auth_service: Arc<AuthService> },
    CliOAuth { cred_paths: Vec<PathBuf> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeminiRequestMode {
    Inline,
    FileUpload,
}

type TranscriptReadyCallback = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone)]
pub struct PendingVoiceNoteInput {
    pub audio: TransientAudioInput,
    pub transcription_runtime: TranscriptionRuntime,
    stt_only_prefix: String,
    transcript_ready: Option<TranscriptReadyCallback>,
}

impl std::fmt::Debug for PendingVoiceNoteInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingVoiceNoteInput")
            .field("audio", &self.audio)
            .field("transcription_runtime", &self.transcription_runtime)
            .field("stt_only_prefix", &self.stt_only_prefix)
            .field("has_transcript_ready_callback", &self.transcript_ready.is_some())
            .finish()
    }
}

impl PendingVoiceNoteInput {
    pub fn new(
        audio: TransientAudioInput,
        transcription_runtime: TranscriptionRuntime,
        stt_only_prefix: impl Into<String>,
    ) -> Self {
        Self {
            audio,
            transcription_runtime,
            stt_only_prefix: stt_only_prefix.into(),
            transcript_ready: None,
        }
    }

    pub fn with_transcript_ready_callback(mut self, callback: TranscriptReadyCallback) -> Self {
        self.transcript_ready = Some(callback);
        self
    }

    pub fn format_stt_only_content(&self, transcript: &str) -> String {
        format!("{}{}", self.stt_only_prefix, transcript)
    }

    pub fn notify_transcript_ready(&self, transcript: &str) {
        if let Some(callback) = self.transcript_ready.as_ref() {
            callback(transcript.to_string());
        }
    }

    pub async fn transcribe_shadow(&self) -> Result<String> {
        transcribe_audio_with_runtime(
            self.audio.bytes.as_ref().clone(),
            &self.audio.file_name,
            &self.transcription_runtime,
        )
        .await
    }
}

fn pending_voice_notes() -> &'static std::sync::Mutex<HashMap<String, PendingVoiceNoteInput>> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, PendingVoiceNoteInput>>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

pub fn register_pending_voice_note(
    message_id: impl Into<String>,
    input: PendingVoiceNoteInput,
) {
    pending_voice_notes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(message_id.into(), input);
}

pub fn take_pending_voice_note(message_id: &str) -> Option<PendingVoiceNoteInput> {
    pending_voice_notes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(message_id)
}

// ── Audio utilities ────────────────────────────────────────────────

fn mime_for_audio(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "flac" => Some("audio/flac"),
        "mp3" | "mpeg" | "mpga" => Some("audio/mpeg"),
        "mp4" | "m4a" => Some("audio/mp4"),
        "ogg" | "oga" => Some("audio/ogg"),
        "opus" => Some("audio/opus"),
        "wav" => Some("audio/wav"),
        "webm" => Some("audio/webm"),
        _ => None,
    }
}

pub fn mime_type_for_audio_file_name(file_name: &str) -> Option<&'static str> {
    let normalized_name = normalize_audio_filename(file_name);
    let extension = normalized_name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("");
    mime_for_audio(extension)
}

pub fn normalize_audio_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase()
}

pub fn transcription_supports_mime(mime: &str) -> bool {
    matches!(
        normalize_audio_mime(mime).as_str(),
        "audio/flac"
            | "audio/mpeg"
            | "audio/mp4"
            | "audio/ogg"
            | "audio/opus"
            | "audio/wav"
            | "audio/webm"
    )
}

fn normalize_audio_filename(file_name: &str) -> String {
    match file_name.rsplit_once('.') {
        Some((stem, ext)) if ext.eq_ignore_ascii_case("oga") => format!("{stem}.ogg"),
        _ => file_name.to_string(),
    }
}

fn validate_audio(audio_data: &[u8], file_name: &str) -> Result<(String, &'static str)> {
    if audio_data.len() > MAX_AUDIO_BYTES {
        bail!(
            "Audio file too large ({} bytes, max {MAX_AUDIO_BYTES})",
            audio_data.len()
        );
    }

    let normalized_name = normalize_audio_filename(file_name);
    let extension = normalized_name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .unwrap_or("");
    let mime = mime_for_audio(extension).ok_or_else(|| {
        anyhow::anyhow!(
            "Unsupported audio format '.{extension}' — accepted: flac, mp3, mp4, mpeg, mpga, m4a, ogg, opus, wav, webm"
        )
    })?;

    Ok((normalized_name, mime))
}

fn normalize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ── TranscriptionProvider trait ────────────────────────────────────

#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn transcribe(&self, audio_data: &[u8], file_name: &str) -> Result<String>;

    fn supported_formats(&self) -> Vec<String> {
        vec![
            "flac", "mp3", "mpeg", "mpga", "mp4", "m4a", "ogg", "oga", "opus", "wav", "webm",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }
}

// ── GroqProvider ───────────────────────────────────────────────────

pub struct GroqProvider {
    api_url: String,
    model: String,
    api_key: String,
    language: Option<String>,
}

impl GroqProvider {
    pub fn from_config(config: &TranscriptionConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .as_deref()
            .and_then(normalize_non_empty)
            .or_else(|| {
                std::env::var("GROQ_API_KEY")
                    .ok()
                    .and_then(|value| normalize_non_empty(&value))
            })
            .context(
                "Missing transcription API key: set [transcription].api_key or GROQ_API_KEY environment variable",
            )?;

        Ok(Self {
            api_url: config.api_url.clone(),
            model: config.model.clone(),
            api_key,
            language: config.language.clone(),
        })
    }
}

#[async_trait]
impl TranscriptionProvider for GroqProvider {
    fn name(&self) -> &str {
        "groq"
    }

    async fn transcribe(&self, audio_data: &[u8], file_name: &str) -> Result<String> {
        let (normalized_name, mime) = validate_audio(audio_data, file_name)?;
        let client = crate::config::build_runtime_proxy_client("transcription.groq");

        let file_part = Part::bytes(audio_data.to_vec())
            .file_name(normalized_name)
            .mime_str(mime)?;

        let mut form = Form::new()
            .part("file", file_part)
            .text("model", self.model.clone())
            .text("response_format", "json");

        if let Some(ref lang) = self.language {
            form = form.text("language", lang.clone());
        }

        let resp = client
            .post(&self.api_url)
            .bearer_auth(&self.api_key)
            .multipart(form)
            .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
            .send()
            .await
            .context("Failed to send transcription request to Groq")?;

        parse_whisper_response(resp).await
    }
}

// ── OpenAiWhisperProvider ──────────────────────────────────────────

pub struct OpenAiWhisperProvider {
    api_key: String,
    model: String,
}

impl OpenAiWhisperProvider {
    pub fn from_config(config: &crate::config::OpenAiSttConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .as_deref()
            .and_then(normalize_non_empty)
            .context("Missing OpenAI STT API key: set [transcription.openai].api_key")?;

        Ok(Self {
            api_key,
            model: config.model.clone(),
        })
    }
}

#[async_trait]
impl TranscriptionProvider for OpenAiWhisperProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn transcribe(&self, audio_data: &[u8], file_name: &str) -> Result<String> {
        let (normalized_name, mime) = validate_audio(audio_data, file_name)?;
        let client = crate::config::build_runtime_proxy_client("transcription.openai");

        let file_part = Part::bytes(audio_data.to_vec())
            .file_name(normalized_name)
            .mime_str(mime)?;

        let form = Form::new()
            .part("file", file_part)
            .text("model", self.model.clone())
            .text("response_format", "json");

        let resp = client
            .post("https://api.openai.com/v1/audio/transcriptions")
            .bearer_auth(&self.api_key)
            .multipart(form)
            .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
            .send()
            .await
            .context("Failed to send transcription request to OpenAI")?;

        parse_whisper_response(resp).await
    }
}

// ── DeepgramProvider ───────────────────────────────────────────────

pub struct DeepgramProvider {
    api_key: String,
    model: String,
}

impl DeepgramProvider {
    pub fn from_config(config: &crate::config::DeepgramSttConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .as_deref()
            .and_then(normalize_non_empty)
            .context("Missing Deepgram API key: set [transcription.deepgram].api_key")?;

        Ok(Self {
            api_key,
            model: config.model.clone(),
        })
    }
}

#[async_trait]
impl TranscriptionProvider for DeepgramProvider {
    fn name(&self) -> &str {
        "deepgram"
    }

    async fn transcribe(&self, audio_data: &[u8], file_name: &str) -> Result<String> {
        let (_, mime) = validate_audio(audio_data, file_name)?;
        let client = crate::config::build_runtime_proxy_client("transcription.deepgram");
        let url = format!(
            "https://api.deepgram.com/v1/listen?model={}&punctuate=true",
            self.model
        );

        let resp = client
            .post(&url)
            .header("Authorization", format!("Token {}", self.api_key))
            .header("Content-Type", mime)
            .body(audio_data.to_vec())
            .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
            .send()
            .await
            .context("Failed to send transcription request to Deepgram")?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Deepgram response")?;

        if !status.is_success() {
            let error_msg = body["err_msg"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("unknown error");
            bail!("Deepgram API error ({}): {}", status, error_msg);
        }

        Ok(body["results"]["channels"][0]["alternatives"][0]["transcript"]
            .as_str()
            .context("Deepgram response missing transcript field")?
            .to_string())
    }
}

// ── AssemblyAiProvider ─────────────────────────────────────────────

pub struct AssemblyAiProvider {
    api_key: String,
}

impl AssemblyAiProvider {
    pub fn from_config(config: &crate::config::AssemblyAiSttConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .as_deref()
            .and_then(normalize_non_empty)
            .context("Missing AssemblyAI API key: set [transcription.assemblyai].api_key")?;

        Ok(Self { api_key })
    }
}

#[async_trait]
impl TranscriptionProvider for AssemblyAiProvider {
    fn name(&self) -> &str {
        "assemblyai"
    }

    async fn transcribe(&self, audio_data: &[u8], file_name: &str) -> Result<String> {
        let _ = validate_audio(audio_data, file_name)?;
        let client = crate::config::build_runtime_proxy_client("transcription.assemblyai");

        let upload_resp = client
            .post("https://api.assemblyai.com/v2/upload")
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/octet-stream")
            .body(audio_data.to_vec())
            .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
            .send()
            .await
            .context("Failed to upload audio to AssemblyAI")?;

        let upload_status = upload_resp.status();
        let upload_body: serde_json::Value = upload_resp
            .json()
            .await
            .context("Failed to parse AssemblyAI upload response")?;

        if !upload_status.is_success() {
            let error_msg = upload_body["error"].as_str().unwrap_or("unknown error");
            bail!("AssemblyAI upload error ({}): {}", upload_status, error_msg);
        }

        let upload_url = upload_body["upload_url"]
            .as_str()
            .context("AssemblyAI upload response missing 'upload_url'")?;

        let create_resp = client
            .post("https://api.assemblyai.com/v2/transcript")
            .header("Authorization", &self.api_key)
            .json(&serde_json::json!({ "audio_url": upload_url }))
            .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
            .send()
            .await
            .context("Failed to create AssemblyAI transcription")?;

        let create_status = create_resp.status();
        let create_body: serde_json::Value = create_resp
            .json()
            .await
            .context("Failed to parse AssemblyAI create response")?;

        if !create_status.is_success() {
            let error_msg = create_body["error"].as_str().unwrap_or("unknown error");
            bail!(
                "AssemblyAI transcription error ({}): {}",
                create_status,
                error_msg
            );
        }

        let transcript_id = create_body["id"]
            .as_str()
            .context("AssemblyAI response missing 'id'")?;
        let poll_url = format!("https://api.assemblyai.com/v2/transcript/{transcript_id}");
        let poll_interval = std::time::Duration::from_secs(3);
        let poll_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);

        while tokio::time::Instant::now() < poll_deadline {
            tokio::time::sleep(poll_interval).await;

            let poll_resp = client
                .get(&poll_url)
                .header("Authorization", &self.api_key)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await
                .context("Failed to poll AssemblyAI transcription")?;

            let poll_status = poll_resp.status();
            let poll_body: serde_json::Value = poll_resp
                .json()
                .await
                .context("Failed to parse AssemblyAI poll response")?;

            if !poll_status.is_success() {
                let error_msg = poll_body["error"].as_str().unwrap_or("unknown poll error");
                bail!("AssemblyAI poll error ({}): {}", poll_status, error_msg);
            }

            match poll_body["status"].as_str().unwrap_or("unknown") {
                "completed" => {
                    return Ok(poll_body["text"]
                        .as_str()
                        .context("AssemblyAI response missing 'text'")?
                        .to_string());
                }
                "error" => {
                    let error_msg = poll_body["error"]
                        .as_str()
                        .unwrap_or("unknown transcription error");
                    bail!("AssemblyAI transcription failed: {}", error_msg);
                }
                _ => {}
            }
        }

        bail!("AssemblyAI transcription timed out after 180s")
    }
}

// ── GoogleSttProvider ──────────────────────────────────────────────

pub struct GoogleSttProvider {
    api_key: String,
    language_code: String,
}

impl GoogleSttProvider {
    pub fn from_config(config: &crate::config::GoogleSttConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .as_deref()
            .and_then(normalize_non_empty)
            .context("Missing Google STT API key: set [transcription.google].api_key")?;

        Ok(Self {
            api_key,
            language_code: config.language_code.clone(),
        })
    }
}

#[async_trait]
impl TranscriptionProvider for GoogleSttProvider {
    fn name(&self) -> &str {
        "google"
    }

    fn supported_formats(&self) -> Vec<String> {
        vec!["flac", "wav", "ogg", "opus", "mp3", "webm"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    async fn transcribe(&self, audio_data: &[u8], file_name: &str) -> Result<String> {
        let (normalized_name, _) = validate_audio(audio_data, file_name)?;
        let client = crate::config::build_runtime_proxy_client("transcription.google");

        let encoding = match normalized_name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("flac") => "FLAC",
            Some("wav") => "LINEAR16",
            Some("ogg" | "opus") => "OGG_OPUS",
            Some("mp3") => "MP3",
            Some("webm") => "WEBM_OPUS",
            Some(ext) => bail!("Google STT does not support '.{ext}' input"),
            None => bail!("Google STT requires a file extension"),
        };

        let audio_content =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, audio_data);

        let request_body = serde_json::json!({
            "config": {
                "encoding": encoding,
                "languageCode": &self.language_code,
                "enableAutomaticPunctuation": true,
            },
            "audio": {
                "content": audio_content,
            }
        });

        let url = format!(
            "https://speech.googleapis.com/v1/speech:recognize?key={}",
            self.api_key
        );

        let resp = client
            .post(&url)
            .json(&request_body)
            .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
            .send()
            .await
            .context("Failed to send transcription request to Google STT")?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Google STT response")?;

        if !status.is_success() {
            let error_msg = body["error"]["message"].as_str().unwrap_or("unknown error");
            bail!("Google STT API error ({}): {}", status, error_msg);
        }

        Ok(body["results"][0]["alternatives"][0]["transcript"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }
}

// ── Shared response parsing ────────────────────────────────────────

async fn parse_whisper_response(resp: reqwest::Response) -> Result<String> {
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse transcription response")?;

    if !status.is_success() {
        let error_msg = body["error"]["message"].as_str().unwrap_or("unknown error");
        bail!("Transcription API error ({}): {}", status, error_msg);
    }

    Ok(body["text"]
        .as_str()
        .context("Transcription response missing 'text' field")?
        .to_string())
}

// ── Gemini STT request/response types ──────────────────────────────

#[derive(Debug, Serialize, Clone)]
struct GeminiGenerateContentRequest {
    contents: Vec<GeminiContent>,
}

#[derive(Debug, Serialize, Clone)]
struct GeminiInternalGenerateContentEnvelope {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(rename = "userPromptId", skip_serializing_if = "Option::is_none")]
    user_prompt_id: Option<String>,
    request: GeminiGenerateContentRequest,
}

#[derive(Debug, Serialize, Clone)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GeminiInlineData,
    },
    FileData {
        #[serde(rename = "fileData")]
        file_data: GeminiFileData,
    },
}

#[derive(Debug, Serialize, Clone)]
struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Debug, Serialize, Clone)]
struct GeminiFileData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    #[serde(rename = "fileUri")]
    file_uri: String,
}

#[derive(Debug, Deserialize)]
struct GeminiGenerateContentResponse {
    #[serde(default)]
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(default)]
    error: Option<GeminiApiError>,
    #[serde(default)]
    response: Option<Box<GeminiGenerateContentResponse>>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiCandidateContent>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidateContent {
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponsePart {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiApiError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GeminiFileUploadEnvelope {
    file: GeminiFileMetadata,
}

#[derive(Debug, Deserialize)]
struct GeminiFileMetadata {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    state: Option<serde_json::Value>,
}

fn build_gemini_stt_prompt(config: &TranscriptionConfig) -> String {
    let mut prompt = String::from(
        "Transcribe the spoken audio and return only the raw transcript text. \
         Do not include timestamps, summaries, speaker labels, headings, or commentary.",
    );

    if let Some(language) = config.language.as_deref().and_then(normalize_non_empty) {
        prompt.push_str(" Language hint: ");
        prompt.push_str(&language);
        prompt.push('.');
    }

    if let Some(initial_prompt) = config
        .initial_prompt
        .as_deref()
        .and_then(normalize_non_empty)
    {
        prompt.push_str(" Vocabulary and context guidance: ");
        prompt.push_str(&initial_prompt);
    }

    prompt
}

fn build_gemini_inline_request(
    prompt: &str,
    mime: &str,
    audio_data: &[u8],
) -> GeminiGenerateContentRequest {
    GeminiGenerateContentRequest {
        contents: vec![GeminiContent {
            role: Some("user".to_string()),
            parts: vec![
                GeminiPart::Text {
                    text: prompt.to_string(),
                },
                GeminiPart::InlineData {
                    inline_data: GeminiInlineData {
                        mime_type: mime.to_string(),
                        data: base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            audio_data,
                        ),
                    },
                },
            ],
        }],
    }
}

fn build_gemini_file_request(
    prompt: &str,
    mime: &str,
    file_uri: &str,
) -> GeminiGenerateContentRequest {
    GeminiGenerateContentRequest {
        contents: vec![GeminiContent {
            role: Some("user".to_string()),
            parts: vec![
                GeminiPart::Text {
                    text: prompt.to_string(),
                },
                GeminiPart::FileData {
                    file_data: GeminiFileData {
                        mime_type: mime.to_string(),
                        file_uri: file_uri.to_string(),
                    },
                },
            ],
        }],
    }
}

fn format_public_model_name(model: &str) -> String {
    if model.trim().starts_with("models/") {
        model.trim().to_string()
    } else {
        format!("models/{}", model.trim())
    }
}

fn format_internal_model_name(model: &str) -> String {
    model
        .trim()
        .strip_prefix("models/")
        .unwrap_or(model.trim())
        .to_string()
}

fn build_gemini_public_generate_url(model: &str) -> String {
    format!(
        "{}/{}:generateContent",
        GEMINI_PUBLIC_API_BASE,
        format_public_model_name(model)
    )
}

fn build_gemini_internal_generate_url() -> String {
    format!("{GEMINI_INTERNAL_API_BASE}:generateContent")
}

fn parse_gemini_response_for_transcript(
    response: GeminiGenerateContentResponse,
) -> Result<String> {
    let GeminiGenerateContentResponse {
        response,
        candidates,
        error,
    } = response;
    let effective = response.map_or(
        GeminiGenerateContentResponse {
            response: None,
            candidates,
            error,
        },
        |inner| *inner,
    );

    if let Some(err) = effective.error {
        bail!("Gemini STT API error: {}", err.message);
    }

    let Some(candidate) = effective
        .candidates
        .unwrap_or_default()
        .into_iter()
        .find(|candidate| {
            candidate
                .content
                .as_ref()
                .is_some_and(|content| !content.parts.is_empty())
        })
    else {
        bail!("Gemini STT returned no transcript text");
    };

    let transcript = candidate
        .content
        .unwrap_or(GeminiCandidateContent { parts: Vec::new() })
        .parts
        .into_iter()
        .filter_map(|part| part.text)
        .collect::<String>()
        .trim()
        .to_string();

    if transcript.is_empty() {
        bail!("Gemini STT returned no transcript text");
    }

    Ok(transcript)
}

fn discover_gemini_cli_oauth_credential_paths() -> Vec<PathBuf> {
    let Some(user_dirs) = UserDirs::new() else {
        return Vec::new();
    };

    let home = user_dirs.home_dir().to_path_buf();
    let mut paths = Vec::new();

    let primary = home.join(".gemini").join("oauth_creds.json");
    if primary.exists() {
        paths.push(primary);
    }

    if let Ok(entries) = std::fs::read_dir(&home) {
        let mut extras: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(".gemini-") && name.ends_with("-home") {
                    let path = entry.path().join(".gemini").join("oauth_creds.json");
                    if path.exists() {
                        return Some(path);
                    }
                }
                None
            })
            .collect();
        extras.sort();
        paths.extend(extras);
    }

    paths
}

#[derive(Debug, Deserialize)]
struct GeminiCliOAuthCreds {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(alias = "idToken", default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(alias = "clientId", default)]
    client_id: Option<String>,
    #[serde(alias = "clientSecret", default)]
    client_secret: Option<String>,
    #[serde(alias = "expiryDate", default)]
    expiry_date: Option<i64>,
    #[serde(default)]
    expiry: Option<String>,
}

#[derive(Clone)]
struct GeminiCliOAuthState {
    access_token: String,
    refresh_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    expiry_millis: Option<i64>,
}

fn load_gemini_cli_oauth_creds(path: &Path) -> Option<GeminiCliOAuthCreds> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn extract_client_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;

    #[derive(Deserialize)]
    struct Claims {
        aud: Option<String>,
        azp: Option<String>,
    }

    let claims: Claims = serde_json::from_slice(&decoded).ok()?;
    claims
        .aud
        .as_deref()
        .and_then(normalize_non_empty)
        .or_else(|| claims.azp.as_deref().and_then(normalize_non_empty))
}

fn build_gemini_cli_oauth_state(creds: GeminiCliOAuthCreds) -> Option<GeminiCliOAuthState> {
    let expiry_millis = creds.expiry_date.or_else(|| {
        creds.expiry.as_deref().and_then(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|parsed| parsed.timestamp_millis())
        })
    });

    let access_token = creds.access_token.as_deref().and_then(normalize_non_empty)?;
    let client_id = std::env::var("GEMINI_OAUTH_CLIENT_ID")
        .ok()
        .and_then(|value| normalize_non_empty(&value))
        .or_else(|| creds.client_id.as_deref().and_then(normalize_non_empty))
        .or_else(|| {
            creds
                .id_token
                .as_deref()
                .and_then(extract_client_id_from_id_token)
        });
    let client_secret = std::env::var("GEMINI_OAUTH_CLIENT_SECRET")
        .ok()
        .and_then(|value| normalize_non_empty(&value))
        .or_else(|| creds.client_secret.as_deref().and_then(normalize_non_empty));

    Some(GeminiCliOAuthState {
        access_token,
        refresh_token: creds.refresh_token,
        client_id,
        client_secret,
        expiry_millis,
    })
}

async fn refresh_gemini_cli_access_token(state: &GeminiCliOAuthState) -> Result<GeminiCliOAuthState> {
    let refresh_token = state
        .refresh_token
        .as_deref()
        .context("Gemini CLI OAuth token expired and no refresh_token is available")?;

    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", state.client_id.as_deref().unwrap_or_default()),
        ("client_secret", state.client_secret.as_deref().unwrap_or_default()),
    ];

    let client = crate::config::build_runtime_proxy_client("transcription.gemini");
    let response = client
        .post(crate::auth::gemini_oauth::GOOGLE_OAUTH_TOKEN_URL)
        .form(&form)
        .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
        .send()
        .await
        .context("Failed to refresh Gemini CLI OAuth token")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "unable to read response body".to_string());

    if !status.is_success() {
        bail!("Gemini CLI OAuth refresh failed ({}): {}", status, body);
    }

    #[derive(Deserialize)]
    struct RefreshResponse {
        access_token: String,
        #[serde(default)]
        expires_in: Option<i64>,
    }

    let refresh: RefreshResponse =
        serde_json::from_str(&body).context("Failed to parse Gemini CLI OAuth refresh response")?;

    Ok(GeminiCliOAuthState {
        access_token: refresh.access_token,
        refresh_token: state.refresh_token.clone(),
        client_id: state.client_id.clone(),
        client_secret: state.client_secret.clone(),
        expiry_millis: refresh
            .expires_in
            .map(|secs| chrono::Utc::now().timestamp_millis().saturating_add(secs * 1000)),
    })
}

async fn resolve_cli_gemini_access_token(cred_paths: &[PathBuf]) -> Result<String> {
    for path in cred_paths {
        let Some(creds) = load_gemini_cli_oauth_creds(path) else {
            continue;
        };
        let Some(mut state) = build_gemini_cli_oauth_state(creds) else {
            continue;
        };

        let now_millis = chrono::Utc::now().timestamp_millis();
        let needs_refresh = state
            .expiry_millis
            .is_some_and(|expiry| expiry <= now_millis.saturating_add(60_000));

        if needs_refresh {
            state = refresh_gemini_cli_access_token(&state).await?;
        }

        return Ok(state.access_token);
    }

    bail!("Gemini CLI OAuth credentials were not found or are unusable")
}

async fn resolve_oauth_project_for_stt(token: &str) -> Result<String> {
    let project_seed = std::env::var("GOOGLE_CLOUD_PROJECT")
        .ok()
        .and_then(|value| normalize_non_empty(&value))
        .or_else(|| {
            std::env::var("GOOGLE_CLOUD_PROJECT_ID")
                .ok()
                .and_then(|value| normalize_non_empty(&value))
        });
    let project_seed_for_request = project_seed.clone();
    let duet_project_for_request = project_seed.clone();

    let client = crate::config::build_runtime_proxy_client("transcription.gemini");
    let response = client
        .post(GEMINI_LOAD_CODE_ASSIST_URL)
        .bearer_auth(token)
        .json(&serde_json::json!({
            "cloudaicompanionProject": project_seed_for_request,
            "metadata": {
                "ideType": "GEMINI_CLI",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI",
                "duetProject": duet_project_for_request,
            }
        }))
        .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
        .send()
        .await
        .context("Failed to resolve Gemini OAuth project context")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if let Some(seed) = project_seed {
            tracing::warn!(
                "Gemini STT loadCodeAssist failed (HTTP {status}); using GOOGLE_CLOUD_PROJECT fallback"
            );
            return Ok(seed);
        }
        bail!("Gemini STT loadCodeAssist failed ({}): {}", status, body);
    }

    #[derive(Deserialize)]
    struct LoadCodeAssistResponse {
        #[serde(rename = "cloudaicompanionProject")]
        cloudaicompanion_project: Option<String>,
    }

    let body: LoadCodeAssistResponse = response
        .json()
        .await
        .context("Failed to parse Gemini loadCodeAssist response")?;

    body.cloudaicompanion_project
        .and_then(|value| normalize_non_empty(&value))
        .or(project_seed)
        .context("Gemini STT loadCodeAssist response missing project context")
}

async fn resolve_gemini_auth_for_stt_with_cli_paths(
    runtime: &TranscriptionRuntime,
    cli_paths: Vec<PathBuf>,
) -> Result<GeminiSttAuth> {
    if let Some(api_key) = runtime
        .config
        .gemini
        .as_ref()
        .and_then(|cfg| cfg.api_key.as_deref())
        .and_then(normalize_non_empty)
    {
        return Ok(GeminiSttAuth::ApiKey(api_key));
    }

    if let Some(api_key) = std::env::var("GEMINI_API_KEY")
        .ok()
        .and_then(|value| normalize_non_empty(&value))
    {
        return Ok(GeminiSttAuth::ApiKey(api_key));
    }

    if let Some(api_key) = std::env::var("GOOGLE_API_KEY")
        .ok()
        .and_then(|value| normalize_non_empty(&value))
    {
        return Ok(GeminiSttAuth::ApiKey(api_key));
    }

    if let Some(auth_service) = runtime.auth_service.clone() {
        if auth_service.get_gemini_profile(None).await?.is_some() {
            return Ok(GeminiSttAuth::ManagedOAuth { auth_service });
        }
    }

    if cli_paths.iter().any(|path| {
        load_gemini_cli_oauth_creds(path)
            .and_then(build_gemini_cli_oauth_state)
            .is_some()
    }) {
        return Ok(GeminiSttAuth::CliOAuth { cred_paths: cli_paths });
    }

    bail!(
        "No Gemini STT authentication found. Configure [transcription.gemini].api_key, GEMINI_API_KEY, GOOGLE_API_KEY, a managed Gemini auth profile, or Gemini CLI OAuth."
    );
}

async fn resolve_gemini_auth_for_stt(runtime: &TranscriptionRuntime) -> Result<GeminiSttAuth> {
    resolve_gemini_auth_for_stt_with_cli_paths(runtime, discover_gemini_cli_oauth_credential_paths())
        .await
}

fn select_gemini_request_mode(
    auth: &GeminiSttAuth,
    estimated_inline_request_bytes: usize,
) -> Result<GeminiRequestMode> {
    if estimated_inline_request_bytes <= GEMINI_INLINE_REQUEST_MAX_BYTES {
        return Ok(GeminiRequestMode::Inline);
    }

    match auth {
        GeminiSttAuth::ApiKey(_) => Ok(GeminiRequestMode::FileUpload),
        GeminiSttAuth::ManagedOAuth { .. } => bail!(
            "Gemini STT inline request body estimated at {} bytes (> {} bytes); managed OAuth supports inline audio only",
            estimated_inline_request_bytes,
            GEMINI_INLINE_REQUEST_MAX_BYTES
        ),
        GeminiSttAuth::CliOAuth { .. } => bail!(
            "Gemini STT inline request body estimated at {} bytes (> {} bytes); CLI OAuth supports inline audio only",
            estimated_inline_request_bytes,
            GEMINI_INLINE_REQUEST_MAX_BYTES
        ),
    }
}

fn extract_gemini_file_state_name(state: Option<&serde_json::Value>) -> Option<String> {
    state.and_then(|value| {
        value
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| {
                value
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
    })
}

async fn parse_gemini_error_response(
    response: reqwest::Response,
    label: &str,
) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| {
            json.get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or(body);

    bail!("Gemini STT {label} failed ({}): {}", status, message)
}

async fn gemini_api_key_generate_content(
    model: &str,
    api_key: &str,
    request: &GeminiGenerateContentRequest,
) -> Result<String> {
    let client = crate::config::build_runtime_proxy_client("transcription.gemini");
    let response = client
        .post(build_gemini_public_generate_url(model))
        .header("x-goog-api-key", api_key)
        .json(request)
        .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
        .send()
        .await
        .context("Failed to send Gemini STT API-key request")?;

    let response =
        parse_gemini_error_response(response, "API-key generateContent request").await?;
    let body: GeminiGenerateContentResponse = response
        .json()
        .await
        .context("Failed to parse Gemini STT response")?;

    parse_gemini_response_for_transcript(body)
}

async fn gemini_oauth_generate_content(
    model: &str,
    token: &str,
    project: &str,
    request: &GeminiGenerateContentRequest,
) -> Result<String> {
    let client = crate::config::build_runtime_proxy_client("transcription.gemini");
    let envelope = GeminiInternalGenerateContentEnvelope {
        model: format_internal_model_name(model),
        project: Some(project.to_string()),
        user_prompt_id: Some(uuid::Uuid::new_v4().to_string()),
        request: request.clone(),
    };

    let response = client
        .post(build_gemini_internal_generate_url())
        .bearer_auth(token)
        .json(&envelope)
        .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
        .send()
        .await
        .context("Failed to send Gemini STT OAuth request")?;

    let response = parse_gemini_error_response(response, "OAuth generateContent request").await?;
    let body: GeminiGenerateContentResponse = response
        .json()
        .await
        .context("Failed to parse Gemini STT OAuth response")?;

    parse_gemini_response_for_transcript(body)
}

async fn gemini_upload_audio_file(
    api_key: &str,
    file_name: &str,
    mime: &str,
    audio_data: &[u8],
) -> Result<GeminiFileMetadata> {
    let client = crate::config::build_runtime_proxy_client("transcription.gemini");

    let start_response = client
        .post(format!("{GEMINI_PUBLIC_UPLOAD_BASE}/files"))
        .header("x-goog-api-key", api_key)
        .header("X-Goog-Upload-Protocol", "resumable")
        .header("X-Goog-Upload-Command", "start")
        .header("X-Goog-Upload-Header-Content-Length", audio_data.len().to_string())
        .header("X-Goog-Upload-Header-Content-Type", mime)
        .json(&serde_json::json!({
            "file": {
                "display_name": file_name,
            }
        }))
        .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
        .send()
        .await
        .context("Failed to start Gemini Files API upload")?;

    let start_response = parse_gemini_error_response(start_response, "Files API upload start").await?;
    let upload_url = start_response
        .headers()
        .get("x-goog-upload-url")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .context("Gemini Files API did not return an upload URL")?;

    let finalize_response = client
        .post(upload_url)
        .header("Content-Length", audio_data.len().to_string())
        .header("X-Goog-Upload-Offset", "0")
        .header("X-Goog-Upload-Command", "upload, finalize")
        .body(audio_data.to_vec())
        .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
        .send()
        .await
        .context("Failed to upload audio bytes to Gemini Files API")?;

    let finalize_response =
        parse_gemini_error_response(finalize_response, "Files API upload finalize").await?;
    let uploaded: GeminiFileUploadEnvelope = finalize_response
        .json()
        .await
        .context("Failed to parse Gemini Files API upload response")?;

    Ok(uploaded.file)
}

async fn gemini_poll_uploaded_file(
    api_key: &str,
    mut file: GeminiFileMetadata,
) -> Result<GeminiFileMetadata> {
    let state = extract_gemini_file_state_name(file.state.as_ref());
    if state
        .as_deref()
        .is_none_or(|value| value.eq_ignore_ascii_case("ACTIVE"))
    {
        return Ok(file);
    }

    let name = file
        .name
        .clone()
        .context("Gemini Files API response missing file name")?;
    let metadata_url = format!("{GEMINI_PUBLIC_API_BASE}/{name}");
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS);
    let client = crate::config::build_runtime_proxy_client("transcription.gemini");

    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let response = client
            .get(&metadata_url)
            .header("x-goog-api-key", api_key)
            .timeout(std::time::Duration::from_secs(TRANSCRIPTION_TIMEOUT_SECS))
            .send()
            .await
            .context("Failed to poll Gemini Files API metadata")?;

        let response = parse_gemini_error_response(response, "Files API poll").await?;
        let envelope: GeminiFileUploadEnvelope = response
            .json()
            .await
            .context("Failed to parse Gemini Files API poll response")?;
        file = envelope.file;

        match extract_gemini_file_state_name(file.state.as_ref()).as_deref() {
            Some("ACTIVE") | None => return Ok(file),
            Some("FAILED") => bail!("Gemini Files API marked the uploaded audio file as FAILED"),
            _ => {}
        }
    }

    bail!("Gemini Files API polling timed out before the uploaded audio became active")
}

// ── GeminiSttProvider ──────────────────────────────────────────────

pub struct GeminiSttProvider {
    runtime: TranscriptionRuntime,
    model: String,
}

impl GeminiSttProvider {
    pub fn from_runtime(runtime: TranscriptionRuntime) -> Result<Self> {
        let model = runtime
            .config
            .gemini
            .as_ref()
            .context("Default transcription provider 'gemini' is not configured. Add [transcription.gemini]")?
            .model
            .clone();

        Ok(Self {
            runtime,
            model,
        })
    }

    fn gemini_config(&self) -> Result<&GeminiSttConfig> {
        self.runtime
            .config
            .gemini
            .as_ref()
            .context("Default transcription provider 'gemini' is not configured. Add [transcription.gemini]")
    }

    async fn transcribe_inline(
        &self,
        auth: &GeminiSttAuth,
        mime: &str,
        audio_data: &[u8],
    ) -> Result<String> {
        let prompt = build_gemini_stt_prompt(&self.runtime.config);
        let request = build_gemini_inline_request(&prompt, mime, audio_data);

        match auth {
            GeminiSttAuth::ApiKey(api_key) => {
                gemini_api_key_generate_content(&self.gemini_config()?.model, api_key, &request)
                    .await
            }
            GeminiSttAuth::ManagedOAuth { auth_service } => {
                let token = auth_service
                    .get_valid_gemini_access_token(None)
                    .await?
                    .context("Managed Gemini auth profile is not available")?;
                let project = resolve_oauth_project_for_stt(&token).await?;
                gemini_oauth_generate_content(
                    &self.gemini_config()?.model,
                    &token,
                    &project,
                    &request,
                )
                .await
            }
            GeminiSttAuth::CliOAuth { cred_paths } => {
                let token = resolve_cli_gemini_access_token(cred_paths).await?;
                let project = resolve_oauth_project_for_stt(&token).await?;
                gemini_oauth_generate_content(
                    &self.gemini_config()?.model,
                    &token,
                    &project,
                    &request,
                )
                .await
            }
        }
    }

    async fn transcribe_with_files_api(
        &self,
        api_key: &str,
        file_name: &str,
        mime: &str,
        audio_data: &[u8],
    ) -> Result<String> {
        let uploaded = gemini_upload_audio_file(api_key, file_name, mime, audio_data).await?;
        let file = gemini_poll_uploaded_file(api_key, uploaded).await?;
        let file_uri = file
            .uri
            .as_deref()
            .and_then(normalize_non_empty)
            .context("Gemini Files API response missing file URI")?;
        let prompt = build_gemini_stt_prompt(&self.runtime.config);
        let request = build_gemini_file_request(&prompt, mime, &file_uri);
        gemini_api_key_generate_content(&self.gemini_config()?.model, api_key, &request).await
    }
}

#[async_trait]
impl TranscriptionProvider for GeminiSttProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    async fn transcribe(&self, audio_data: &[u8], file_name: &str) -> Result<String> {
        let (_, mime) = validate_audio(audio_data, file_name)?;
        let auth = resolve_gemini_auth_for_stt(&self.runtime).await?;
        let prompt = build_gemini_stt_prompt(&self.runtime.config);
        let inline_request = build_gemini_inline_request(&prompt, mime, audio_data);

        let estimated_bytes = match &auth {
            GeminiSttAuth::ApiKey(_) => serde_json::to_vec(&inline_request)
                .context("Failed to serialize Gemini STT inline request for size estimation")?
                .len(),
            GeminiSttAuth::ManagedOAuth { .. } | GeminiSttAuth::CliOAuth { .. } => {
                let envelope = GeminiInternalGenerateContentEnvelope {
                    model: format_internal_model_name(&self.model),
                    project: Some("project-estimate".to_string()),
                    user_prompt_id: Some("estimate".to_string()),
                    request: inline_request.clone(),
                };
                serde_json::to_vec(&envelope)
                    .context("Failed to serialize Gemini STT OAuth request for size estimation")?
                    .len()
            }
        };

        match select_gemini_request_mode(&auth, estimated_bytes)? {
            GeminiRequestMode::Inline => self.transcribe_inline(&auth, mime, audio_data).await,
            GeminiRequestMode::FileUpload => match auth {
                GeminiSttAuth::ApiKey(api_key) => {
                    self.transcribe_with_files_api(&api_key, file_name, mime, audio_data)
                        .await
                }
                GeminiSttAuth::ManagedOAuth { .. } | GeminiSttAuth::CliOAuth { .. } => {
                    unreachable!("OAuth should have been rejected by select_gemini_request_mode")
                }
            },
        }
    }
}

// ── TranscriptionManager ───────────────────────────────────────────

pub struct TranscriptionManager {
    providers: HashMap<String, Box<dyn TranscriptionProvider>>,
    default_provider: String,
}

impl TranscriptionManager {
    pub fn new(config: &TranscriptionConfig) -> Result<Self> {
        Self::new_with_runtime(TranscriptionRuntime::from_config(config.clone()))
    }

    pub fn new_with_runtime(runtime: TranscriptionRuntime) -> Result<Self> {
        let mut providers: HashMap<String, Box<dyn TranscriptionProvider>> = HashMap::new();
        let config = &runtime.config;

        if let Ok(groq) = GroqProvider::from_config(config) {
            providers.insert("groq".to_string(), Box::new(groq));
        }

        if let Some(ref openai_cfg) = config.openai {
            if let Ok(provider) = OpenAiWhisperProvider::from_config(openai_cfg) {
                providers.insert("openai".to_string(), Box::new(provider));
            }
        }

        if let Some(ref deepgram_cfg) = config.deepgram {
            if let Ok(provider) = DeepgramProvider::from_config(deepgram_cfg) {
                providers.insert("deepgram".to_string(), Box::new(provider));
            }
        }

        if let Some(ref assemblyai_cfg) = config.assemblyai {
            if let Ok(provider) = AssemblyAiProvider::from_config(assemblyai_cfg) {
                providers.insert("assemblyai".to_string(), Box::new(provider));
            }
        }

        if let Some(ref google_cfg) = config.google {
            if let Ok(provider) = GoogleSttProvider::from_config(google_cfg) {
                providers.insert("google".to_string(), Box::new(provider));
            }
        }

        if config.gemini.is_some() {
            providers.insert(
                "gemini".to_string(),
                Box::new(GeminiSttProvider::from_runtime(runtime.clone())?),
            );
        }

        let default_provider = config.default_provider.clone();

        if config.enabled && !providers.contains_key(&default_provider) {
            let available: Vec<&str> = providers.keys().map(|key| key.as_str()).collect();
            bail!(
                "Default transcription provider '{}' is not configured. Available: {available:?}",
                default_provider
            );
        }

        Ok(Self {
            providers,
            default_provider,
        })
    }

    pub async fn transcribe(&self, audio_data: &[u8], file_name: &str) -> Result<String> {
        self.transcribe_with_provider(audio_data, file_name, &self.default_provider)
            .await
    }

    pub async fn transcribe_with_provider(
        &self,
        audio_data: &[u8],
        file_name: &str,
        provider: &str,
    ) -> Result<String> {
        let provider = self.providers.get(provider).ok_or_else(|| {
            let available: Vec<&str> = self.providers.keys().map(|key| key.as_str()).collect();
            anyhow::anyhow!(
                "Transcription provider '{provider}' not configured. Available: {available:?}"
            )
        })?;

        provider.transcribe(audio_data, file_name).await
    }

    pub fn available_providers(&self) -> Vec<&str> {
        self.providers.keys().map(|key| key.as_str()).collect()
    }
}

pub fn validate_transcription_runtime(runtime: &TranscriptionRuntime) -> Result<()> {
    match runtime.config.default_provider.as_str() {
        "groq" => {
            let _ = GroqProvider::from_config(&runtime.config)?;
        }
        "openai" => {
            let openai_cfg = runtime.config.openai.as_ref().context(
                "Default transcription provider 'openai' is not configured. Add [transcription.openai]",
            )?;
            let _ = OpenAiWhisperProvider::from_config(openai_cfg)?;
        }
        "deepgram" => {
            let deepgram_cfg = runtime.config.deepgram.as_ref().context(
                "Default transcription provider 'deepgram' is not configured. Add [transcription.deepgram]",
            )?;
            let _ = DeepgramProvider::from_config(deepgram_cfg)?;
        }
        "assemblyai" => {
            let assemblyai_cfg = runtime.config.assemblyai.as_ref().context(
                "Default transcription provider 'assemblyai' is not configured. Add [transcription.assemblyai]",
            )?;
            let _ = AssemblyAiProvider::from_config(assemblyai_cfg)?;
        }
        "google" => {
            let google_cfg = runtime.config.google.as_ref().context(
                "Default transcription provider 'google' is not configured. Add [transcription.google]",
            )?;
            let _ = GoogleSttProvider::from_config(google_cfg)?;
        }
        "gemini" => {
            let _ = GeminiSttProvider::from_runtime(runtime.clone())?;
        }
        other => bail!("Unsupported transcription provider '{other}'"),
    }

    Ok(())
}

// ── Compatibility entry points ────────────────────────────────────

pub async fn transcribe_audio(
    audio_data: Vec<u8>,
    file_name: &str,
    config: &TranscriptionConfig,
) -> Result<String> {
    transcribe_audio_with_runtime(
        audio_data,
        file_name,
        &TranscriptionRuntime::from_config(config.clone()),
    )
    .await
}

pub async fn transcribe_audio_with_runtime(
    audio_data: Vec<u8>,
    file_name: &str,
    runtime: &TranscriptionRuntime,
) -> Result<String> {
    validate_audio(&audio_data, file_name)?;

    match runtime.config.default_provider.as_str() {
        "groq" => GroqProvider::from_config(&runtime.config)?
            .transcribe(&audio_data, file_name)
            .await,
        "openai" => {
            let openai_cfg = runtime.config.openai.as_ref().context(
                "Default transcription provider 'openai' is not configured. Add [transcription.openai]",
            )?;
            OpenAiWhisperProvider::from_config(openai_cfg)?
                .transcribe(&audio_data, file_name)
                .await
        }
        "deepgram" => {
            let deepgram_cfg = runtime.config.deepgram.as_ref().context(
                "Default transcription provider 'deepgram' is not configured. Add [transcription.deepgram]",
            )?;
            DeepgramProvider::from_config(deepgram_cfg)?
                .transcribe(&audio_data, file_name)
                .await
        }
        "assemblyai" => {
            let assemblyai_cfg = runtime.config.assemblyai.as_ref().context(
                "Default transcription provider 'assemblyai' is not configured. Add [transcription.assemblyai]",
            )?;
            AssemblyAiProvider::from_config(assemblyai_cfg)?
                .transcribe(&audio_data, file_name)
                .await
        }
        "google" => {
            let google_cfg = runtime.config.google.as_ref().context(
                "Default transcription provider 'google' is not configured. Add [transcription.google]",
            )?;
            GoogleSttProvider::from_config(google_cfg)?
                .transcribe(&audio_data, file_name)
                .await
        }
        "gemini" => GeminiSttProvider::from_runtime(runtime.clone())?
            .transcribe(&audio_data, file_name)
            .await,
        other => bail!("Unsupported transcription provider '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_test_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap()
    }

    struct EnvVarRestore {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }

        fn remove(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, original }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            if let Some(ref original) = self.original {
                std::env::set_var(self.key, original);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn gemini_test_config() -> TranscriptionConfig {
        let mut config = TranscriptionConfig::default();
        config.enabled = true;
        config.default_provider = "gemini".to_string();
        config.gemini = Some(crate::config::GeminiSttConfig {
            api_key: None,
            model: "gemini-3-flash-preview".to_string(),
        });
        config
    }

    async fn test_auth_service_with_gemini_profile() -> Arc<AuthService> {
        let dir = std::env::temp_dir().join(format!(
            "zeroclaw_gemini_auth_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let service = Arc::new(AuthService::new(&dir, false));
        service
            .store_gemini_tokens(
                "default",
                crate::auth::profiles::TokenSet {
                    access_token: "managed-token".to_string(),
                    refresh_token: Some("managed-refresh".to_string()),
                    id_token: None,
                    expires_at: None,
                    token_type: Some("Bearer".to_string()),
                    scope: None,
                },
                Some("test@example.com".to_string()),
                true,
            )
            .await
            .unwrap();
        service
    }

    #[tokio::test]
    async fn rejects_oversized_audio() {
        let big = vec![0u8; MAX_AUDIO_BYTES + 1];
        let config = TranscriptionConfig::default();

        let err = transcribe_audio(big, "test.ogg", &config).await.unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[tokio::test]
    async fn rejects_missing_api_key() {
        let _env_lock = env_test_lock();
        let _groq = EnvVarRestore::remove("GROQ_API_KEY");
        let _openai = EnvVarRestore::remove("OPENAI_API_KEY");
        let _transcription = EnvVarRestore::remove("TRANSCRIPTION_API_KEY");

        let data = vec![0u8; 100];
        let config = TranscriptionConfig::default();

        let err = transcribe_audio(data, "test.ogg", &config).await.unwrap_err();
        assert!(err.to_string().contains("transcription API key"));
    }

    #[tokio::test]
    async fn uses_config_api_key_without_groq_env() {
        let _env_lock = env_test_lock();
        let _groq = EnvVarRestore::remove("GROQ_API_KEY");

        let data = vec![0u8; 100];
        let mut config = TranscriptionConfig::default();
        config.api_key = Some("transcription-key".to_string());

        let err = transcribe_audio(data, "recording.aac", &config)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Unsupported audio format"));
    }

    #[tokio::test]
    async fn openai_default_provider_uses_openai_config() {
        let data = vec![0u8; 100];
        let mut config = TranscriptionConfig::default();
        config.default_provider = "openai".to_string();
        config.openai = Some(crate::config::OpenAiSttConfig {
            api_key: None,
            model: "gpt-4o-mini-transcribe".to_string(),
        });

        let err = transcribe_audio(data, "test.ogg", &config).await.unwrap_err();
        assert!(err.to_string().contains("[transcription.openai].api_key"));
    }

    #[test]
    fn mime_for_audio_maps_accepted_formats() {
        let cases = [
            ("flac", "audio/flac"),
            ("mp3", "audio/mpeg"),
            ("mpeg", "audio/mpeg"),
            ("mpga", "audio/mpeg"),
            ("mp4", "audio/mp4"),
            ("m4a", "audio/mp4"),
            ("ogg", "audio/ogg"),
            ("oga", "audio/ogg"),
            ("opus", "audio/opus"),
            ("wav", "audio/wav"),
            ("webm", "audio/webm"),
        ];

        for (ext, expected) in cases {
            assert_eq!(mime_for_audio(ext), Some(expected));
        }
    }

    #[test]
    fn mime_for_audio_case_insensitive() {
        assert_eq!(mime_for_audio("OGG"), Some("audio/ogg"));
        assert_eq!(mime_for_audio("MP3"), Some("audio/mpeg"));
        assert_eq!(mime_for_audio("Opus"), Some("audio/opus"));
    }

    #[test]
    fn mime_for_audio_rejects_unknown() {
        assert_eq!(mime_for_audio("txt"), None);
        assert_eq!(mime_for_audio("pdf"), None);
        assert_eq!(mime_for_audio("aac"), None);
        assert_eq!(mime_for_audio(""), None);
    }

    #[test]
    fn normalize_audio_filename_rewrites_oga() {
        assert_eq!(normalize_audio_filename("voice.oga"), "voice.ogg");
        assert_eq!(normalize_audio_filename("file.OGA"), "file.ogg");
    }

    #[test]
    fn normalize_audio_filename_preserves_accepted() {
        assert_eq!(normalize_audio_filename("voice.ogg"), "voice.ogg");
        assert_eq!(normalize_audio_filename("track.mp3"), "track.mp3");
        assert_eq!(normalize_audio_filename("clip.opus"), "clip.opus");
    }

    #[test]
    fn normalize_audio_filename_no_extension() {
        assert_eq!(normalize_audio_filename("voice"), "voice");
    }

    #[tokio::test]
    async fn rejects_unsupported_audio_format() {
        let data = vec![0u8; 100];
        let config = TranscriptionConfig::default();

        let err = transcribe_audio(data, "recording.aac", &config)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Unsupported audio format"));
        assert!(msg.contains(".aac"));
    }

    #[test]
    fn manager_creation_with_default_config() {
        let _env_lock = env_test_lock();
        let _groq = EnvVarRestore::remove("GROQ_API_KEY");

        let config = TranscriptionConfig::default();
        let manager = TranscriptionManager::new(&config).unwrap();
        assert_eq!(manager.default_provider, "groq");
        assert!(manager.providers.is_empty());
    }

    #[test]
    fn manager_registers_groq_with_key() {
        let _env_lock = env_test_lock();
        let _groq = EnvVarRestore::remove("GROQ_API_KEY");

        let mut config = TranscriptionConfig::default();
        config.api_key = Some("test-groq-key".to_string());

        let manager = TranscriptionManager::new(&config).unwrap();
        assert!(manager.providers.contains_key("groq"));
    }

    #[test]
    fn manager_registers_multiple_providers() {
        let _env_lock = env_test_lock();
        let _groq = EnvVarRestore::remove("GROQ_API_KEY");

        let mut config = TranscriptionConfig::default();
        config.api_key = Some("test-groq-key".to_string());
        config.openai = Some(crate::config::OpenAiSttConfig {
            api_key: Some("test-openai-key".to_string()),
            model: "whisper-1".to_string(),
        });
        config.deepgram = Some(crate::config::DeepgramSttConfig {
            api_key: Some("test-deepgram-key".to_string()),
            model: "nova-2".to_string(),
        });
        config.gemini = Some(crate::config::GeminiSttConfig {
            api_key: None,
            model: "gemini-3-flash-preview".to_string(),
        });

        let manager = TranscriptionManager::new(&config).unwrap();
        assert!(manager.providers.contains_key("groq"));
        assert!(manager.providers.contains_key("openai"));
        assert!(manager.providers.contains_key("deepgram"));
        assert!(manager.providers.contains_key("gemini"));
    }

    #[tokio::test]
    async fn manager_rejects_unconfigured_provider() {
        let _env_lock = env_test_lock();
        let _groq = EnvVarRestore::remove("GROQ_API_KEY");

        let mut config = TranscriptionConfig::default();
        config.api_key = Some("test-groq-key".to_string());

        let manager = TranscriptionManager::new(&config).unwrap();
        let err = manager
            .transcribe_with_provider(&[0u8; 100], "test.ogg", "nonexistent")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn manager_default_provider_from_config() {
        let _env_lock = env_test_lock();
        let _groq = EnvVarRestore::remove("GROQ_API_KEY");

        let mut config = TranscriptionConfig::default();
        config.default_provider = "openai".to_string();
        config.openai = Some(crate::config::OpenAiSttConfig {
            api_key: Some("test-openai-key".to_string()),
            model: "whisper-1".to_string(),
        });

        let manager = TranscriptionManager::new(&config).unwrap();
        assert_eq!(manager.default_provider, "openai");
    }

    #[test]
    fn validate_audio_rejects_oversized() {
        let big = vec![0u8; MAX_AUDIO_BYTES + 1];
        let err = validate_audio(&big, "test.ogg").unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn validate_audio_rejects_unsupported_format() {
        let data = vec![0u8; 100];
        let err = validate_audio(&data, "test.aac").unwrap_err();
        assert!(err.to_string().contains("Unsupported audio format"));
    }

    #[test]
    fn validate_audio_accepts_supported_format() {
        let data = vec![0u8; 100];
        let (name, mime) = validate_audio(&data, "test.ogg").unwrap();
        assert_eq!(name, "test.ogg");
        assert_eq!(mime, "audio/ogg");
    }

    #[test]
    fn validate_audio_normalizes_oga() {
        let data = vec![0u8; 100];
        let (name, mime) = validate_audio(&data, "voice.oga").unwrap();
        assert_eq!(name, "voice.ogg");
        assert_eq!(mime, "audio/ogg");
    }

    #[test]
    fn backward_compat_config_defaults_unchanged() {
        let config = TranscriptionConfig::default();
        assert!(!config.enabled);
        assert!(config.api_key.is_none());
        assert!(config.api_url.contains("groq.com"));
        assert_eq!(config.model, "whisper-large-v3-turbo");
        assert_eq!(config.default_provider, "groq");
        assert!(config.openai.is_none());
        assert!(config.deepgram.is_none());
        assert!(config.assemblyai.is_none());
        assert!(config.google.is_none());
        assert!(config.gemini.is_none());
    }

    #[tokio::test]
    async fn gemini_auth_prefers_config_key_over_env() {
        let _env_lock = env_test_lock();
        let _gemini = EnvVarRestore::set("GEMINI_API_KEY", "env-gemini");
        let _google = EnvVarRestore::set("GOOGLE_API_KEY", "env-google");

        let mut runtime = TranscriptionRuntime::from_config(gemini_test_config());
        runtime.config.gemini.as_mut().unwrap().api_key = Some("config-key".to_string());

        let auth = resolve_gemini_auth_for_stt_with_cli_paths(&runtime, Vec::new())
            .await
            .unwrap();

        assert!(matches!(auth, GeminiSttAuth::ApiKey(ref key) if key == "config-key"));
    }

    #[tokio::test]
    async fn gemini_auth_prefers_gemini_env_over_google_env() {
        let _env_lock = env_test_lock();
        let _gemini = EnvVarRestore::set("GEMINI_API_KEY", "env-gemini");
        let _google = EnvVarRestore::set("GOOGLE_API_KEY", "env-google");

        let runtime = TranscriptionRuntime::from_config(gemini_test_config());
        let auth = resolve_gemini_auth_for_stt_with_cli_paths(&runtime, Vec::new())
            .await
            .unwrap();

        assert!(matches!(auth, GeminiSttAuth::ApiKey(ref key) if key == "env-gemini"));
    }

    #[tokio::test]
    async fn gemini_auth_prefers_api_key_over_managed_auth() {
        let _env_lock = env_test_lock();
        let _gemini = EnvVarRestore::set("GEMINI_API_KEY", "env-gemini");
        let service = test_auth_service_with_gemini_profile().await;
        let mut runtime = TranscriptionRuntime::from_config(gemini_test_config());
        runtime.auth_service = Some(service);

        let auth = resolve_gemini_auth_for_stt_with_cli_paths(&runtime, Vec::new())
            .await
            .unwrap();

        assert!(matches!(auth, GeminiSttAuth::ApiKey(ref key) if key == "env-gemini"));
    }

    #[tokio::test]
    async fn gemini_auth_prefers_google_api_key_over_managed_auth() {
        let _env_lock = env_test_lock();
        let _gemini = EnvVarRestore::remove("GEMINI_API_KEY");
        let _google = EnvVarRestore::set("GOOGLE_API_KEY", "env-google");
        let service = test_auth_service_with_gemini_profile().await;
        let mut runtime = TranscriptionRuntime::from_config(gemini_test_config());
        runtime.auth_service = Some(service);

        let auth = resolve_gemini_auth_for_stt_with_cli_paths(&runtime, Vec::new())
            .await
            .unwrap();

        assert!(matches!(auth, GeminiSttAuth::ApiKey(ref key) if key == "env-google"));
    }

    #[tokio::test]
    async fn gemini_auth_prefers_managed_auth_over_cli_oauth() {
        let _env_lock = env_test_lock();
        let _gemini = EnvVarRestore::remove("GEMINI_API_KEY");
        let _google = EnvVarRestore::remove("GOOGLE_API_KEY");
        let service = test_auth_service_with_gemini_profile().await;
        let mut runtime = TranscriptionRuntime::from_config(gemini_test_config());
        runtime.auth_service = Some(service);
        let cli_path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            cli_path.path(),
            r#"{"access_token":"cli-token","refresh_token":"refresh"}"#,
        )
        .unwrap();

        let auth = resolve_gemini_auth_for_stt_with_cli_paths(
            &runtime,
            vec![cli_path.path().to_path_buf()],
        )
        .await
        .unwrap();

        assert!(matches!(auth, GeminiSttAuth::ManagedOAuth { .. }));
    }

    #[test]
    fn gemini_size_routing_uses_inline_at_threshold() {
        let auth = GeminiSttAuth::ApiKey("key".to_string());
        assert_eq!(
            select_gemini_request_mode(&auth, GEMINI_INLINE_REQUEST_MAX_BYTES - 1).unwrap(),
            GeminiRequestMode::Inline
        );
        assert_eq!(
            select_gemini_request_mode(&auth, GEMINI_INLINE_REQUEST_MAX_BYTES).unwrap(),
            GeminiRequestMode::Inline
        );
    }

    #[test]
    fn gemini_size_routing_uses_upload_over_threshold_for_api_key() {
        let auth = GeminiSttAuth::ApiKey("key".to_string());
        assert_eq!(
            select_gemini_request_mode(&auth, GEMINI_INLINE_REQUEST_MAX_BYTES + 1).unwrap(),
            GeminiRequestMode::FileUpload
        );
    }

    #[test]
    fn gemini_size_routing_rejects_oauth_over_threshold() {
        let auth = GeminiSttAuth::CliOAuth {
            cred_paths: vec![PathBuf::from("oauth_creds.json")],
        };
        let err = select_gemini_request_mode(&auth, GEMINI_INLINE_REQUEST_MAX_BYTES + 1)
            .unwrap_err();
        assert!(err.to_string().contains("supports inline audio only"));
    }

    #[test]
    fn gemini_response_uses_first_non_empty_candidate_only() {
        let response: GeminiGenerateContentResponse = serde_json::from_value(serde_json::json!({
            "candidates": [
                { "content": { "parts": [] } },
                { "content": { "parts": [ { "text": "Hello" }, { "inlineData": { "mimeType": "audio/ogg", "data": "abc" } }, { "text": " world" } ] } },
                { "content": { "parts": [ { "text": "ignored" } ] } }
            ]
        }))
        .unwrap();

        let transcript = parse_gemini_response_for_transcript(response).unwrap();
        assert_eq!(transcript, "Hello world");
    }

    #[test]
    fn gemini_response_returns_clean_error_when_no_text() {
        let response: GeminiGenerateContentResponse = serde_json::from_value(serde_json::json!({
            "candidates": [
                { "content": { "parts": [ { "inlineData": { "mimeType": "audio/ogg", "data": "abc" } } ] } }
            ]
        }))
        .unwrap();

        let err = parse_gemini_response_for_transcript(response).unwrap_err();
        assert_eq!(err.to_string(), "Gemini STT returned no transcript text");
    }

    #[test]
    fn gemini_request_uses_configured_model_unchanged() {
        let mut config = gemini_test_config();
        config.gemini.as_mut().unwrap().model = "gemini-3.1-pro-preview".to_string();
        let provider =
            GeminiSttProvider::from_runtime(TranscriptionRuntime::from_config(config)).unwrap();
        assert_eq!(provider.model, "gemini-3.1-pro-preview");
    }

    #[test]
    fn google_provider_registration_remains_separate_from_gemini() {
        let _env_lock = env_test_lock();
        let _groq = EnvVarRestore::remove("GROQ_API_KEY");

        let mut config = TranscriptionConfig::default();
        config.enabled = true;
        config.default_provider = "google".to_string();
        config.google = Some(crate::config::GoogleSttConfig {
            api_key: Some("google-stt-key".to_string()),
            language_code: "en-US".to_string(),
        });

        let manager = TranscriptionManager::new(&config).unwrap();
        assert!(manager.providers.contains_key("google"));
        assert!(!manager.providers.contains_key("gemini"));
    }
}
