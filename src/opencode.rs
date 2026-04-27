use std::{sync::Arc, time::Duration};

use opencode_rs::{
    Client,
    server::{ManagedServer, ServerOptions},
    types::{
        message::{Message, Part, PromptPart, PromptRequest},
        project::ModelRef,
        session::CreateSessionRequest,
    },
};
use serde_json::Value;
use tokio::{sync::Mutex, time::timeout};

use crate::{
    error::{AppError, Result},
    model::{ScoredCandidate, YoutubeTrack},
};

const DEFAULT_SERVER: &str = "http://127.0.0.1:4096";
const TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub struct OpencodeResolver {
    client: Client,
    config: OpencodeConfig,
    _server: Option<Arc<Mutex<ManagedServer>>>,
}

#[derive(Debug, Clone, Default)]
pub struct OpencodeConfig {
    base_url: Option<String>,
    model: Option<String>,
    variant: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpencodeResolution {
    pub candidate: Option<ScoredCandidate>,
    pub reason: Option<String>,
}

impl OpencodeResolution {
    pub fn rejection_reason(&self) -> String {
        self.reason
            .as_ref()
            .map(|reason| format!("opencode did not choose a track: {reason}"))
            .unwrap_or_else(|| "opencode did not choose a track".to_string())
    }
}

impl OpencodeResolver {
    pub async fn connect(config: OpencodeConfig) -> Result<Self> {
        let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_SERVER);

        if let Ok(client) = healthy_client(base_url).await {
            return Ok(Self {
                client,
                config,
                _server: None,
            });
        }
        if config.base_url.is_some() {
            return Err(AppError::InvalidInput(format!(
                "no healthy opencode server at {base_url}"
            )));
        }

        let server = ManagedServer::start(
            ServerOptions::new()
                .hostname("127.0.0.1")
                .startup_timeout_ms(10_000),
        )
        .await
        .map_err(|err| AppError::InvalidInput(format!("failed to start opencode serve: {err}")))?;
        let client = client(server.url().as_str())?;

        Ok(Self {
            client,
            config,
            _server: Some(Arc::new(Mutex::new(server))),
        })
    }

    pub async fn resolve(
        &self,
        youtube: &YoutubeTrack,
        candidates: &[ScoredCandidate],
    ) -> Result<OpencodeResolution> {
        if candidates.is_empty() {
            return Ok(no_choice("no Spotify candidates were available"));
        }

        let session = self
            .client
            .sessions()
            .create(&CreateSessionRequest {
                title: Some(format!("[y2s] ambiguous match #{}", youtube.index + 1)),
                ..Default::default()
            })
            .await
            .map_err(|err| {
                AppError::InvalidInput(format!("opencode session create failed: {err}"))
            })?;

        let result = self
            .resolve_in_session(&session.id, youtube, candidates)
            .await;
        let _ = self.client.sessions().delete(&session.id).await;
        result
    }

    async fn resolve_in_session(
        &self,
        session_id: &str,
        youtube: &YoutubeTrack,
        candidates: &[ScoredCandidate],
    ) -> Result<OpencodeResolution> {
        let request = self
            .config
            .prompt_request(resolver_prompt(youtube, candidates));
        let response = timeout(TIMEOUT, self.client.messages().prompt(session_id, &request))
            .await
            .map_err(|_| AppError::InvalidInput("opencode resolver timed out".to_string()))?
            .map_err(|err| AppError::InvalidInput(format!("opencode prompt failed: {err}")))?;

        let text = match text_from_value(&response.extra) {
            Some(text) => text.to_string(),
            None => self.wait_for_text(session_id).await?,
        };
        let (choice, reason) = parse_resolution(&text, candidates.len())?;

        Ok(OpencodeResolution {
            candidate: choice.map(|index| candidates[index].clone()),
            reason,
        })
    }

    async fn wait_for_text(&self, session_id: &str) -> Result<String> {
        for _ in 0..(TIMEOUT.as_secs() * 4) {
            let messages = self
                .client
                .messages()
                .list(session_id)
                .await
                .map_err(|err| {
                    AppError::InvalidInput(format!("opencode message list failed: {err}"))
                })?;
            if let Some(text) = assistant_text(&messages) {
                return Ok(text.to_string());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        Err(AppError::InvalidInput(
            "opencode produced no assistant text".to_string(),
        ))
    }
}

impl OpencodeConfig {
    pub fn new(base_url: Option<String>, model: Option<String>, variant: Option<String>) -> Self {
        Self {
            base_url,
            model,
            variant,
        }
    }

    fn prompt_request(&self, text: String) -> PromptRequest {
        PromptRequest {
            parts: vec![PromptPart::Text {
                text,
                synthetic: None,
                ignored: None,
                metadata: None,
            }],
            message_id: None,
            model: self.model.as_deref().map(model_ref),
            agent: None,
            no_reply: None,
            system: None,
            variant: self.variant.clone(),
        }
    }
}

async fn healthy_client(base_url: &str) -> Result<Client> {
    let client = client(base_url)?;
    client
        .global()
        .health()
        .await
        .ok()
        .filter(|healthy| *healthy)
        .map(|_| client)
        .ok_or_else(|| {
            AppError::InvalidInput(format!("opencode server at {base_url} is not healthy"))
        })
}

fn client(base_url: &str) -> Result<Client> {
    Client::builder()
        .base_url(base_url)
        .directory(std::env::current_dir()?.to_string_lossy())
        .timeout_secs(TIMEOUT.as_secs())
        .build()
        .map_err(|err| AppError::InvalidInput(format!("opencode client setup failed: {err}")))
}

fn no_choice(reason: impl Into<String>) -> OpencodeResolution {
    OpencodeResolution {
        candidate: None,
        reason: Some(reason.into()),
    }
}

fn text_from_value(value: &Value) -> Option<&str> {
    value.get("parts")?.as_array()?.iter().find_map(|part| {
        (part.get("type").and_then(Value::as_str) == Some("text"))
            .then(|| {
                part.get("text")
                    .or_else(|| part.get("content"))
                    .and_then(Value::as_str)
            })
            .flatten()
            .filter(|text| !text.trim().is_empty())
    })
}

fn assistant_text(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| message.role() == "assistant")?
        .parts
        .iter()
        .find_map(|part| match part {
            Part::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
}

fn resolver_prompt(youtube: &YoutubeTrack, candidates: &[ScoredCandidate]) -> String {
    let youtube_duration = youtube
        .duration_ms
        .map(duration)
        .unwrap_or("unknown".into());
    let album = youtube.album.as_deref().unwrap_or("unknown");
    let mut prompt = format!(
        "You are resolving an ambiguous YouTube to Spotify track match.\n\
Choose the Spotify candidate that is most likely the same song/release a human would want in a synced playlist. Avoid false positives, but do not return null just because YouTube duration/title metadata looks like an upload edit when there is one clear official Spotify version by the same artist/title.\n\
Prefer exact artist/title, same release/album when known, and closest duration. Never choose covers or tribute versions over the original artist. Do not choose live/acoustic/radio/edit/remix/slowed/sped variants unless the YouTube metadata indicates that variant, but if the exact YouTube-only edit is unavailable, choose the closest official original by the same artist/title instead of null. A remastered original by the exact artist is acceptable when no non-remastered original candidate is present. If multiple candidates are duplicate releases of the same recording, prefer the album/LP release over a single unless YouTube metadata indicates the single.\n\
Treat official audio, visualizer, visualette, lyric video, from-album/video labels, braces, emojis, and punctuation differences as non-musical presentation noise.\n\
Return a candidate number when the candidate is the best available Spotify equivalent. Return null only when choosing would risk a wrong artist, cover, tribute, wrong song, unrelated classical recording, or multi-track YouTube upload that cannot map to one Spotify track.\n\n\
Return only one JSON object with this schema:\n\
{{\"choice\": <1-based candidate number or null>, \"reason\": \"short reason\"}}\n\n\
YouTube track:\n\
Index: {}\n\
Title: {}\n\
Artists: {}\n\
Album: {}\n\
Duration: {}\n\
Video ID: {}\n\n\
Spotify candidates:\n",
        youtube.index + 1,
        youtube.title,
        youtube.artist_display(),
        album,
        youtube_duration,
        youtube.video_id
    );

    for (index, candidate) in candidates.iter().enumerate() {
        prompt.push_str(&format!(
            "{}. {} - {} | album: {} | duration: {} | score: {:.1} | uri: {} | scoring: {}\n",
            index + 1,
            candidate.track.artist_display(),
            candidate.track.title,
            candidate.track.album.as_deref().unwrap_or("unknown"),
            candidate
                .track
                .duration_ms
                .map(duration)
                .unwrap_or("unknown".into()),
            candidate.score,
            candidate.track.uri,
            candidate.reason
        ));
    }

    prompt
}

fn parse_resolution(text: &str, candidate_count: usize) -> Result<(Option<usize>, Option<String>)> {
    let Some(json) = extract_json_object(text) else {
        return Ok((None, Some("response did not contain a JSON object".into())));
    };
    let value: Value = serde_json::from_str(json)?;
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);

    let Some(choice) = value.get("choice") else {
        return Ok((None, reason));
    };
    if choice.is_null() {
        return Ok((None, reason));
    }
    let Some(choice) = choice.as_u64() else {
        return Ok((
            None,
            reason.or_else(|| Some("choice was not a number".into())),
        ));
    };
    if choice == 0 || choice as usize > candidate_count {
        return Ok((
            None,
            reason.or_else(|| Some(format!("choice {choice} was out of range"))),
        ));
    }

    Ok((Some(choice as usize - 1), reason))
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (start <= end).then_some(&text[start..=end])
}

fn model_ref(value: &str) -> ModelRef {
    let (provider_id, model_id) = value
        .split_once('/')
        .map(|(provider, model)| (Some(provider.to_string()), Some(model.to_string())))
        .unwrap_or_else(|| (None, Some(value.to_string())));
    ModelRef {
        provider_id,
        model_id,
        extra: Value::Null,
    }
}

fn duration(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use crate::matching::normalize;

    use super::*;

    #[test]
    fn prompt_request_sets_model_variant_and_no_agent() {
        let request = OpencodeConfig::new(
            None,
            Some("opencode/minimax-m2.5-free".into()),
            Some("build".into()),
        )
        .prompt_request("prompt".into());
        let model = request.model.unwrap();
        assert_eq!(model.provider_id.as_deref(), Some("opencode"));
        assert_eq!(model.model_id.as_deref(), Some("minimax-m2.5-free"));
        assert_eq!(request.variant.as_deref(), Some("build"));
        assert!(request.agent.is_none());
    }

    #[test]
    fn parses_choice() {
        let parsed = parse_resolution(r#"{"choice":2,"reason":"exact"}"#, 3).unwrap();
        assert_eq!(parsed, (Some(1), Some("exact".into())));
    }

    #[test]
    fn parses_null_choice_reason() {
        let parsed = parse_resolution(r#"{"choice":null,"reason":"unsafe variant"}"#, 3).unwrap();
        assert_eq!(parsed, (None, Some("unsafe variant".into())));
    }

    #[test]
    fn extracts_text_from_prompt_response_parts() {
        let extra = serde_json::json!({
            "parts": [{"type": "text", "text": "{\"choice\":1,\"reason\":\"ok\"}"}]
        });
        assert_eq!(
            text_from_value(&extra).unwrap(),
            r#"{"choice":1,"reason":"ok"}"#
        );
    }

    #[test]
    fn extracts_last_assistant_text_from_messages() {
        let messages = serde_json::from_value::<Vec<Message>>(serde_json::json!([{
            "info": {"id": "msg_1", "sessionId": "ses_1", "role": "assistant", "time": {"created": 1}},
            "parts": [{"type": "text", "text": "{\"choice\":1,\"reason\":\"ok\"}"}]
        }]))
        .unwrap();

        assert_eq!(
            assistant_text(&messages).unwrap(),
            r#"{"choice":1,"reason":"ok"}"#
        );
    }

    #[test]
    fn normalize_import_stays_public_for_prompt_context() {
        assert_eq!(normalize("Café"), "cafe");
    }
}
