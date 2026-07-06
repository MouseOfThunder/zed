use language_model::{
    LanguageModelRequest, LanguageModelToolChoice, LanguageModelToolResultContent, MessageContent,
    Role,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct LocalMlxRequest {
    pub model: String,
    pub messages: Vec<LocalMlxMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<ExtraBody>,
}

#[derive(Debug, Serialize)]
pub struct ExtraBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolDef {
    Function { function: FunctionDef },
}

#[derive(Debug, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    Required,
    None,
}

#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum LocalMlxMessage {
    User {
        content: String,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        tool_calls: Vec<ToolCallChunk>,
    },
    System {
        content: String,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
}

#[derive(Debug, Serialize)]
pub struct ToolCallChunk {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Serialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

impl LocalMlxRequest {
    pub fn from_language_model_request(
        request: LanguageModelRequest,
        model_name: &str,
        max_output_tokens: u64,
        enable_thinking: Option<bool>,
        repeat_penalty: Option<f32>,
        top_p: Option<f32>,
        top_k: Option<u32>,
    ) -> Self {
        let mut messages: Vec<LocalMlxMessage> = Vec::new();

        for msg in request.messages {
            // Check for tool results in any message
            let tool_results: Vec<_> = msg
                .content
                .iter()
                .filter_map(|c| match c {
                    MessageContent::ToolResult(r) => Some(r.clone()),
                    _ => None,
                })
                .collect();

            if !tool_results.is_empty() {
                for result in tool_results {
                    let text = result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            LanguageModelToolResultContent::Text(t) => Some(t.to_string()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    messages.push(LocalMlxMessage::Tool {
                        content: text,
                        tool_call_id: result.tool_use_id.to_string(),
                    });
                }
                continue;
            }

            match msg.role {
                Role::User => {
                    let text = msg
                        .content
                        .into_iter()
                        .filter_map(|c| match c {
                            MessageContent::Text(t) => Some(t),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.trim().is_empty() {
                        messages.push(LocalMlxMessage::User { content: text });
                    }
                }
                Role::System => {
                    let text = msg
                        .content
                        .into_iter()
                        .filter_map(|c| match c {
                            MessageContent::Text(t) => Some(t),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        messages.push(LocalMlxMessage::System { content: text });
                    }
                }
                Role::Assistant => {
                    let text = msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            MessageContent::Text(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");

                    let tool_calls: Vec<ToolCallChunk> = msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            MessageContent::ToolUse(tool_use) => Some(ToolCallChunk {
                                id: tool_use.id.to_string(),
                                call_type: "function".to_string(),
                                function: ToolCallFunction {
                                    name: tool_use.name.to_string(),
                                    arguments: serde_json::to_string(&tool_use.input)
                                        .unwrap_or_default(),
                                },
                            }),
                            _ => None,
                        })
                        .collect();

                    if !text.is_empty() || !tool_calls.is_empty() {
                        messages.push(LocalMlxMessage::Assistant {
                            content: if text.is_empty() { None } else { Some(text) },
                            tool_calls,
                        });
                    }
                }
            }
        }

        let tools: Vec<ToolDef> = request
            .tools
            .into_iter()
            .map(|tool| ToolDef::Function {
                function: FunctionDef {
                    name: tool.name,
                    description: Some(tool.description),
                    parameters: Some(tool.input_schema),
                },
            })
            .collect();

        let tool_choice = request.tool_choice.map(|choice| match choice {
            LanguageModelToolChoice::Auto => ToolChoice::Auto,
            LanguageModelToolChoice::Any => ToolChoice::Required,
            LanguageModelToolChoice::None => ToolChoice::None,
        });

        let extra_body = if enable_thinking.is_some() {
            Some(ExtraBody { enable_thinking })
        } else {
            None
        };

        Self {
            model: model_name.to_string(),
            messages,
            stream: true,
            max_tokens: Some(max_output_tokens),
            stop: request.stop,
            temperature: request.temperature,
            top_p,
            top_k,
            repeat_penalty,
            frequency_penalty: None,
            tools,
            tool_choice,
            extra_body,
        }
    }
}
