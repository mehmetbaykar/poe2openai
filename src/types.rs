use poe_api_process::types::{ChatTool, ChatToolCall};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<ExtraBody>,
}

#[derive(Deserialize)]
pub struct StreamOptions {
    pub include_usage: Option<bool>,
}

#[derive(Deserialize)]
pub struct ThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<i32>,
}

#[derive(Deserialize)]
pub struct ExtraBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub google: Option<GoogleConfig>,
}

#[derive(Deserialize)]
pub struct GoogleConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<GoogleThinkingConfig>,
}

#[derive(Deserialize)]
pub struct GoogleThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<i32>,
}

// 定義支援 OpenAI content 格式的 enum (String 或陣列)
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum OpenAiContent {
    Text(String),
    Multi(Vec<OpenAiContentItem>),
}

// 定義 OpenAI content 陣列內的項目類型
#[derive(Debug, Clone)]
pub enum OpenAiContentItem {
    Text { text: String },
    ImageUrl { image_url: ImageUrlContent },
}

impl<'de> serde::Deserialize<'de> for OpenAiContentItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        
        // Check if it has a type field
        let item_type = value.get("type")
            .and_then(|v| v.as_str());
        
        // If no type field, try to infer from available fields
        let inferred_type = if item_type.is_none() {
            if value.get("image_url").is_some() {
                Some("image_url")
            } else if value.get("text").is_some() {
                Some("text")
            } else {
                None
            }
        } else {
            item_type
        };
        
        match inferred_type {
            Some("text") => {
                let text = value.get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| serde::de::Error::missing_field("text"))?;
                Ok(OpenAiContentItem::Text { text: text.to_string() })
            },
            Some("image_url") => {
                let image_url_value = value.get("image_url")
                    .ok_or_else(|| serde::de::Error::missing_field("image_url"))?;
                let image_url = ImageUrlContent::deserialize(image_url_value)
                    .map_err(|e| serde::de::Error::custom(e))?;
                Ok(OpenAiContentItem::ImageUrl { image_url })
            },
            _ => {
                Err(serde::de::Error::unknown_variant(
                    item_type.unwrap_or("unknown"), 
                    &["text", "image_url"]
                ))
            }
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<OpenAiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct OpenAIErrorResponse {
    pub error: OpenAIError,
}

#[derive(Serialize, Clone, Debug)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) custom_models: Option<Vec<CustomModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) api_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) use_v1_api: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;


    #[test]
    fn test_openai_content_item_inference() {
        // Test that OpenAiContentItem correctly infers type from available fields
        let text_item_json = r#"{
            "text": "Hello, world!"
        }"#;

        let result: Result<OpenAiContentItem, _> = serde_json::from_str(text_item_json);
        assert!(result.is_ok(), "OpenAiContentItem without type field should infer as text");

        let item = result.unwrap();
        match item {
            OpenAiContentItem::Text { text } => {
                assert_eq!(text, "Hello, world!");
            }
            _ => panic!("Should have been parsed as text"),
        }

        let image_item_json = r#"{
            "image_url": {
                "url": "https://example.com/image.jpg"
            }
        }"#;

        let result: Result<OpenAiContentItem, _> = serde_json::from_str(image_item_json);
        assert!(result.is_ok(), "OpenAiContentItem without type field should infer as image_url");

        let item = result.unwrap();
        match item {
            OpenAiContentItem::ImageUrl { image_url } => {
                assert_eq!(image_url.url, "https://example.com/image.jpg");
            }
            _ => panic!("Should have been parsed as image_url"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct CustomModel {
    pub(crate) id: String,
    pub(crate) created: Option<i64>,
    pub(crate) owned_by: Option<String>,
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
