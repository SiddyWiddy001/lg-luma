//! Cloud Brain — NVIDIA NIM Qwen integration with Search & Conversation Memory.
//!
//! Sends the user's transcript to the NVIDIA NIM API (model: qwen/qwen3.8-27b)
//! with conversation history and installed app context. Supports three actions:
//! SPEAK, LAUNCH, SEARCH. SEARCH triggers a web search + second Qwen call.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const NIM_ENDPOINT: &str = "https://integrate.api.nvidia.com/v1/chat/completions";
const NIM_MODEL: &str = "qwen/qwen3.8-27b";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
const CONVERSATION_TTL: Duration = Duration::from_secs(60);
const MAX_HISTORY_MESSAGES: usize = 10;

/// The API key file path (never log or expose its contents).
const NIM_KEY_PATH: &str = "/var/lib/luma-home/.nim_key";

/// DuckDuckGo HTML endpoint for web search.
const DDG_HTML_URL: &str = "https://html.duckduckgo.com/html/";

/// A single message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    /// Milliseconds since UNIX_EPOCH (serializable).
    pub timestamp_ms: u64,
}

/// The parsed action from Qwen.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceAction {
    Speak(String),
    Launch(String),
    Search(String),
}

/// Errors that can occur during cloud query.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("API key file not found at {0}")]
    KeyMissing(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("API error: {0}")]
    Api(String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Search error: {0}")]
    Search(String),
}

#[derive(Serialize)]
struct NimRequest {
    model: String,
    messages: Vec<NimMessage>,
    max_tokens: u32,
    temperature: f32,
    stream: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct NimMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct NimResponse {
    choices: Vec<NimChoice>,
}

#[derive(Deserialize)]
struct NimChoice {
    message: NimResponseMessage,
}

#[derive(Deserialize)]
struct NimResponseMessage {
    content: String,
}

/// Shared conversation state (history + last activity).
#[derive(Clone)]
pub struct ConversationState {
    history: Arc<Mutex<Vec<ConversationMessage>>>,
}

impl ConversationState {
    pub fn new() -> Self {
        Self {
            history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn add_user(&self, content: String) {
        let mut history = self.history.lock().await;
        history.push(ConversationMessage {
            role: "user".to_string(),
            content,
            timestamp_ms: now_ms(),
        });
        Self::trim_history(&mut history);
    }

    pub async fn add_assistant(&self, content: String) {
        let mut history = self.history.lock().await;
        history.push(ConversationMessage {
            role: "assistant".to_string(),
            content,
            timestamp_ms: now_ms(),
        });
        Self::trim_history(&mut history);
    }

    pub async fn get_messages(&self) -> Vec<NimMessage> {
        let history = self.history.lock().await;
        history
            .iter()
            .map(|m| NimMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect()
    }

    pub async fn maybe_clear_expired(&self) {
        let mut history = self.history.lock().await;
        if let Some(last) = history.last() {
            let now = now_ms();
            if now.saturating_sub(last.timestamp_ms) > CONVERSATION_TTL.as_millis() as u64 {
                history.clear();
            }
        }
    }

    fn trim_history(history: &mut Vec<ConversationMessage>) {
        if history.len() > MAX_HISTORY_MESSAGES * 2 {
            let drain_count = history.len() - MAX_HISTORY_MESSAGES * 2;
            history.drain(0..drain_count);
        }
    }
}

/// Query the NVIDIA NIM Qwen model with transcript, app list, and conversation history.
pub async fn query_qwen(
    transcript: String,
    app_list: Vec<(String, String)>,
    conversation: &ConversationState,
) -> Result<VoiceAction, CloudError> {
    conversation.maybe_clear_expired().await;
    conversation.add_user(transcript.clone()).await;

    let api_key = std::fs::read_to_string(NIM_KEY_PATH)
        .map_err(|_| CloudError::KeyMissing(NIM_KEY_PATH.to_string()))?
        .trim()
        .to_string();

    if api_key.is_empty() {
        return Err(CloudError::KeyMissing(NIM_KEY_PATH.to_string()));
    }

    let _client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))?;

    let app_context = if app_list.is_empty() {
        "No apps installed.".to_string()
    } else {
        app_list
            .iter()
            .map(|(name, id)| format!("{} -> {}", name, id))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let system_prompt = format!(
        r#"You are a TV voice assistant. You must reply with EXACTLY ONE of these formats:
1. SPEAK: [Your 1-sentence answer]
2. LAUNCH: [app_id]
3. SEARCH: [search query]
Do not include any other text, reasoning, or markdown.
Here is the list of installed apps on the TV: {}"#,
        app_context
    );

    let history = conversation.get_messages().await;

    let request = NimRequest {
        model: NIM_MODEL.to_string(),
        messages: {
            let mut msgs = vec![NimMessage {
                role: "system".to_string(),
                content: system_prompt,
            }];
            msgs.extend(history);
            msgs
        },
        max_tokens: 64,
        temperature: 0.1,
        stream: false,
    };

    let response = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))?
        .post(NIM_ENDPOINT)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .map_err(|e| CloudError::Network(e.to_string()))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(CloudError::Api(format!("HTTP {}: {}", status, body)));
    }

    let nim_resp: NimResponse = response
        .json()
        .await
        .map_err(|e| CloudError::Parse(e.to_string()))?;

    let content = nim_resp
        .choices
        .first()
        .map(|c| c.message.content.trim())
        .ok_or_else(|| CloudError::Parse("empty response".to_string()))?;

    let action = parse_action(content)?;

    if let VoiceAction::Speak(ref text) = action {
        conversation.add_assistant(text.clone()).await;
    }

    Ok(action)
}

/// Execute a web search via DuckDuckGo HTML scraping.
pub async fn web_search(query: &str) -> Result<Vec<String>, CloudError> {
    let client = Client::builder()
        .timeout(SEARCH_TIMEOUT)
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))?;

    let response = client
        .post(DDG_HTML_URL)
        .form(&[("q", query)])
        .header("User-Agent", "Mozilla/5.0 (compatible; LumaVoice/1.0)")
        .send()
        .await
        .map_err(|e| CloudError::Search(e.to_string()))?;

    let html = response
        .text()
        .await
        .map_err(|e| CloudError::Search(e.to_string()))?;

    let snippets = extract_snippets(&html);

    if snippets.is_empty() {
        return Err(CloudError::Search("no results found".to_string()));
    }

    Ok(snippets.into_iter().take(5).collect())
}

/// Extract text snippets from DuckDuckGo HTML.
fn extract_snippets(html: &str) -> Vec<String> {
    let mut snippets = Vec::new();
    let mut start = 0;
    while let Some(pos) = html[start..].find("class=\"result__snippet\"") {
        let abs_pos = start + pos;
        if let Some(gt) = html[abs_pos..].find('>') {
            let content_start = abs_pos + gt + 1;
            if let Some(lt) = html[content_start..].find('<') {
                let snippet = html[content_start..content_start + lt].trim();
                if !snippet.is_empty() && snippet.len() > 20 {
                    snippets.push(snippet.to_string());
                }
            }
        }
        start = abs_pos + 1;
        if snippets.len() >= 5 {
            break;
        }
    }
    snippets
}

/// Second Qwen call: summarize search results into a SPEAK action.
pub async fn summarize_search(
    original_query: &str,
    snippets: Vec<String>,
    conversation: &ConversationState,
) -> Result<VoiceAction, CloudError> {
    let api_key = std::fs::read_to_string(NIM_KEY_PATH)
        .map_err(|_| CloudError::KeyMissing(NIM_KEY_PATH.to_string()))?
        .trim()
        .to_string();

    if api_key.is_empty() {
        return Err(CloudError::KeyMissing(NIM_KEY_PATH.to_string()));
    }

    let _client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))?;

    let snippets_text = snippets
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n");

    let system_prompt = r#"You are a TV voice assistant. Based on the search results provided, reply with EXACTLY ONE format:
SPEAK: [Your 1-sentence summary answer]
Do not include any other text, reasoning, or markdown."#;

    let user_prompt = format!(
        "Here are the search results for '{}':\n{}\n\nBased on this, provide a 1-sentence answer.",
        original_query, snippets_text
    );

    let history = conversation.get_messages().await;

    let request = NimRequest {
        model: NIM_MODEL.to_string(),
        messages: {
            let mut msgs = vec![
                NimMessage {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
                },
                NimMessage {
                    role: "user".to_string(),
                    content: user_prompt,
                },
            ];
            msgs.extend(history);
            msgs
        },
        max_tokens: 64,
        temperature: 0.1,
        stream: false,
    };

    let response = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))?
        .post(NIM_ENDPOINT)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .map_err(|e| CloudError::Network(e.to_string()))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(CloudError::Api(format!("HTTP {}: {}", status, body)));
    }

    let nim_resp: NimResponse = response
        .json()
        .await
        .map_err(|e| CloudError::Parse(e.to_string()))?;

    let content = nim_resp
        .choices
        .first()
        .map(|c| c.message.content.trim())
        .ok_or_else(|| CloudError::Parse("empty response".to_string()))?;

    if let Some(text) = content.strip_prefix("SPEAK:") {
        let text = text.trim();
        if text.is_empty() {
            return Err(CloudError::Parse("empty SPEAK payload".to_string()));
        }
        conversation.add_assistant(text.to_string()).await;
        Ok(VoiceAction::Speak(text.to_string()))
    } else {
        Err(CloudError::Parse(format!(
            "summarization must return SPEAK:, got: {}",
            content
        )))
    }
}

fn parse_action(line: &str) -> Result<VoiceAction, CloudError> {
    if let Some(text) = line.strip_prefix("SPEAK:") {
        let text = text.trim();
        if text.is_empty() {
            return Err(CloudError::Parse("empty SPEAK payload".to_string()));
        }
        Ok(VoiceAction::Speak(text.to_string()))
    } else if let Some(app_id) = line.strip_prefix("LAUNCH:") {
        let app_id = app_id.trim();
        if app_id.is_empty() {
            return Err(CloudError::Parse("empty LAUNCH payload".to_string()));
        }
        Ok(VoiceAction::Launch(app_id.to_string()))
    } else if let Some(query) = line.strip_prefix("SEARCH:") {
        let query = query.trim();
        if query.is_empty() {
            return Err(CloudError::Parse("empty SEARCH payload".to_string()));
        }
        Ok(VoiceAction::Search(query.to_string()))
    } else {
        Err(CloudError::Parse(format!(
            "unrecognized format (must start with SPEAK:, LAUNCH:, or SEARCH:): {}",
            line
        )))
    }
}

/// Helper to get current time in milliseconds since UNIX_EPOCH.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
