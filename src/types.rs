use poe_api_process::types::{ChatTool, ChatToolCall};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Deserialize, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<HashMap<String, f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_headers: Option<serde_json::Value>,
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

#[derive(Deserialize, Serialize)]
pub struct StreamOptions {
    pub include_usage: Option<bool>,
}

#[derive(Deserialize, Serialize)]
pub struct ThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<i32>,
}

#[derive(Deserialize, Serialize)]
pub struct ExtraBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub google: Option<GoogleConfig>,
}

#[derive(Deserialize, Serialize)]
pub struct GoogleConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<GoogleThinkingConfig>,
}

#[derive(Deserialize, Serialize)]
pub struct GoogleThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<i32>,
}

// Define enum supporting OpenAI content format (String or array)
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum OpenAiContent {
    Text(String),
    Multi(Vec<OpenAiContentItem>),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum OpenAiContentItem {
    Text {
        #[serde(default)]
        r#type: Option<String>,
        text: String,
        #[serde(flatten, default)]
        extra: HashMap<String, serde_json::Value>,
    },
    ImageUrl {
        #[serde(default)]
        r#type: Option<String>,
        image_url: ImageUrlContent,
        #[serde(flatten, default)]
        extra: HashMap<String, serde_json::Value>,
    },
    ToolResult {
        #[serde(default)]
        r#type: Option<String>,
        id: Option<String>,
        tool_call_id: Option<String>,
        content: serde_json::Value,
        #[serde(flatten, default)]
        extra: HashMap<String, serde_json::Value>,
    },
    InputAudio {
        #[serde(default)]
        r#type: Option<String>,
        audio: serde_json::Value,
        #[serde(flatten, default)]
        extra: HashMap<String, serde_json::Value>,
    },
    Other(serde_json::Value),
}

impl OpenAiContentItem {
    #[allow(dead_code)]
    pub fn content_type(&self) -> Option<&str> {
        match self {
            OpenAiContentItem::Text { r#type, .. }
            | OpenAiContentItem::ImageUrl { r#type, .. }
            | OpenAiContentItem::ToolResult { r#type, .. }
            | OpenAiContentItem::InputAudio { r#type, .. } => r#type.as_deref(),
            OpenAiContentItem::Other(value) => value.get("type").and_then(|v| v.as_str()),
        }
    }

    #[allow(dead_code)]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            OpenAiContentItem::Text { text, .. } => Some(text.as_str()),
            OpenAiContentItem::Other(value) => value.get("text").and_then(|v| v.as_str()),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn as_image_url(&self) -> Option<&ImageUrlContent> {
        match self {
            OpenAiContentItem::ImageUrl { image_url, .. } => Some(image_url),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn as_image_url_mut(&mut self) -> Option<&mut ImageUrlContent> {
        match self {
            OpenAiContentItem::ImageUrl { image_url, .. } => Some(image_url),
            _ => None,
        }
    }
}

// Define content structure of image_url
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ImageUrlContent {
    pub url: String,
    #[serde(flatten, default)]
    pub extra: HashMap<String, serde_json::Value>,
}

// Update Message structure to use new OpenAiContent
#[derive(Deserialize, Serialize, Clone)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<OpenAiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
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
    pub tool_calls: Option<Vec<ChunkToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ChunkToolCall {
    pub index: u32,
    #[serde(flatten)]
    pub call: ChatToolCall,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_content_block_without_type() {
        let payload = json!({
            "model": "gpt-4o",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "text": "Ping without explicit type"
                        }
                    ]
                }
            ]
        });

        let req: ChatCompletionRequest = serde_json::from_value(payload).expect("valid request");
        assert_eq!(req.messages.len(), 1);
        match req.messages[0].content.as_ref().expect("content present") {
            OpenAiContent::Multi(items) => {
                assert_eq!(items.len(), 1);
                match &items[0] {
                    OpenAiContentItem::Text { r#type, text, .. } => {
                        assert!(r#type.is_none());
                        assert_eq!(text, "Ping without explicit type");
                    }
                    other => panic!("unexpected content variant: {:?}", other),
                }
            }
            other => panic!("unexpected content shape: {:?}", other),
        }
    }

    #[test]
    fn deserializes_tools_missing_required_fields() {
        let payload = json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": "hello"
            }],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "search",
                        "parameters": {
                            "properties": {
                                "query": { "type": "string" }
                            }
                        }
                    }
                },
                {
                    "type": "function"
                }
            ]
        });

        let req: ChatCompletionRequest = serde_json::from_value(payload).expect("valid request");
        let tools = req.tools.expect("tools present");
        assert_eq!(tools.len(), 2);

        let search_tool = &tools[0];
        assert_eq!(search_tool.function.name, "search");
        let params = search_tool
            .function
            .parameters
            .as_ref()
            .expect("parameters present");
        assert!(params.required.is_empty());
        assert!(params.r#type.is_none());

        let fallback_tool = &tools[1];
        assert!(fallback_tool.function.name.is_empty());
    }
}
