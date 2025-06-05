use std::collections::HashMap;

use poe_api_process::types::{ChatTool, ChatToolCall};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub prompt_tokens_details: PromptTokensDetails,
}

#[derive(Serialize)]
pub struct PromptTokensDetails {
    pub cached_tokens: u32,
}

#[derive(Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<HashMap<String, f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatTool>>,
    pub stream_options: Option<StreamOptions>,
}

#[derive(Deserialize)]
pub struct StreamOptions {
    pub include_usage: Option<bool>,
}

// 定義支援 OpenAI content 格式的 enum (String 或陣列)
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum OpenAiContent {
    Text(String),
    Multi(Vec<OpenAiContentItem>),
}

// 定義 OpenAI content 陣列內的項目類型
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum OpenAiContentItem {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlContent },
}

// 定義 image_url 的內容結構
#[derive(Debug, Deserialize, Clone)]
pub struct ImageUrlContent {
    pub url: String,
    // 可擴展其他欄位如 detail 等
}

// 更新 Message 結構使用新的 OpenAiContent
#[derive(Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: OpenAiContent,
}

#[derive(Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct CompletionChoice {
    pub index: u32,
    pub message: CompletionMessage,
    pub logprobs: Option<serde_json::Value>,
    pub finish_reason: Option<String>,
}

#[derive(Serialize)]
pub struct CompletionMessage {
    pub role: String,
    pub content: String,
    pub refusal: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
}

#[derive(Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
}

#[derive(Serialize)]
pub struct Choice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Serialize)]
pub struct Delta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub refusal: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
}

#[derive(Serialize)]
pub struct OpenAIErrorResponse {
    pub error: OpenAIError,
}

#[derive(Serialize)]
pub struct OpenAIError {
    pub message: String,
    pub r#type: String,
    pub code: String,
    pub param: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Config {
    pub(crate) enable: Option<bool>,
    pub(crate) models: std::collections::HashMap<String, ModelConfig>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub(crate) struct ModelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mapping: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replace_response: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enable: Option<bool>,
}
