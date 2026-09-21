use std::collections::BTreeMap;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use nanocodex::{Model, TurnUsage};
use serde::{Deserialize, Serialize};

pub type Metadata = BTreeMap<String, String>;
pub type ApiResult<T> = Result<T, ApiError>;

pub struct ApiError(pub StatusCode, pub &'static str, pub String);
impl ApiError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message.into(),
        )
    }
    pub fn missing() -> Self {
        Self(
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found".into(),
        )
    }
    pub fn conflict(message: &str) -> Self {
        Self(StatusCode::CONFLICT, "conflict", message.into())
    }
    pub fn internal() -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "The service could not complete this operation".into(),
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            message: String,
            r#type: &'static str,
            code: &'static str,
            param: Option<String>,
        }
        #[derive(Serialize)]
        struct Envelope {
            error: ErrorBody,
        }
        (
            self.0,
            Json(Envelope {
                error: ErrorBody {
                    message: self.2,
                    r#type: self.1,
                    code: self.1,
                    param: None,
                },
            }),
        )
            .into_response()
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Environment {
    #[serde(rename = "none")]
    None,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    pub environment: Environment,
    pub agent: AgentInput,
    pub input: Option<Input>,
    #[serde(default)]
    pub stream: bool,
    pub metadata: Option<Metadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInput {
    pub model: String,
    pub instructions: Option<String>,
}
impl AgentInput {
    pub fn validate(&self) -> ApiResult<Model> {
        if self.instructions.as_ref().is_some_and(|s| s.len() > 32_768) {
            return Err(ApiError::invalid("Instructions exceed 32768 bytes"));
        }
        let model: Model = self.model.parse().map_err(ApiError::invalid)?;
        if model.as_str() != self.model {
            return Err(ApiError::invalid("Use a full model ID"));
        }
        Ok(model)
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Messages(Vec<InputMessage>),
}
impl Input {
    pub fn messages(self) -> ApiResult<Vec<InputMessage>> {
        let messages = match self {
            Self::Text(text) => vec![InputMessage {
                r#type: MessageType::Message,
                role: UserRole::User,
                content: vec![InputText::Text { text }],
            }],
            Self::Messages(messages) => messages,
        };
        let size: usize = messages
            .iter()
            .flat_map(|m| &m.content)
            .map(|c| c.text().len())
            .sum();
        if messages.is_empty()
            || messages.len() > 32
            || size > 65_536
            || messages.iter().any(|m| {
                m.content.is_empty() || m.content.iter().all(|c| c.text().trim().is_empty())
            })
        {
            return Err(ApiError::invalid(
                "Supply 1 to 32 nonempty text messages, at most 65536 bytes in total",
            ));
        }
        Ok(messages)
    }
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    #[default]
    Message,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    User,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputMessage {
    #[serde(default)]
    pub r#type: MessageType,
    pub role: UserRole,
    pub content: Vec<InputText>,
}
impl InputMessage {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .map(InputText::text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum InputText {
    #[serde(rename = "input_text")]
    Text { text: String },
}
impl InputText {
    pub fn text(&self) -> &str {
        match self {
            Self::Text { text } => text,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitEvents {
    pub events: Vec<InputEvent>,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum InputEvent {
    #[serde(rename = "agent.session.input.message")]
    Message { input: Vec<InputMessage> },
    #[serde(rename = "agent.session.input.cancel")]
    Cancel,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSession {
    pub metadata: Option<Metadata>,
}
pub fn metadata(value: Option<Metadata>) -> ApiResult<Metadata> {
    let value = value.unwrap_or_default();
    if value.len() > 16 || value.iter().any(|(k, v)| k.len() > 64 || v.len() > 512) {
        return Err(ApiError::invalid(
            "Metadata allows 16 fields, 64-byte keys, and 512-byte values",
        ));
    }
    Ok(value)
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    pub after: Option<String>,
    pub limit: Option<usize>,
    pub order: Option<Order>,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Asc,
    Desc,
}
#[derive(Serialize)]
pub struct Page<T> {
    pub object: &'static str,
    pub data: Vec<T>,
    pub has_more: bool,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
}
pub trait Identified {
    fn id(&self) -> &str;
}
impl PageQuery {
    pub fn page<T: Identified>(&self, mut items: Vec<T>) -> ApiResult<Page<T>> {
        let limit = self.limit.unwrap_or(20);
        if !(1..=100).contains(&limit) {
            return Err(ApiError::invalid("limit must be from 1 to 100"));
        }
        if !matches!(self.order, Some(Order::Asc)) {
            items.reverse();
        }
        if let Some(after) = &self.after {
            let index = items
                .iter()
                .position(|i| i.id() == after)
                .ok_or_else(|| ApiError::invalid("Unknown after cursor"))?;
            items.drain(..=index);
        }
        let has_more = items.len() > limit;
        items.truncate(limit);
        Ok(Page {
            object: "list",
            first_id: items.first().map(|i| i.id().into()),
            last_id: items.last().map(|i| i.id().into()),
            data: items,
            has_more,
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Agent {
    pub id: String,
    pub model: String,
    pub instructions: Option<String>,
    pub name: Option<String>,
    pub tools: Vec<Unsupported>,
    pub multi_agent: MultiAgent,
    pub reasoning: Reasoning,
    pub service_tier: String,
    pub text: TextConfig,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct MultiAgent {
    pub enabled: bool,
    pub max_concurrent_subagents: Option<u32>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Reasoning {
    pub effort: String,
    pub summary: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct TextConfig {
    pub format: TextFormat,
    pub verbosity: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct TextFormat {
    pub r#type: String,
}
// Empty arrays are part of the supported wire contract. This type cannot have elements.
#[derive(Clone, Deserialize, Serialize)]
pub enum Unsupported {}
#[derive(Clone, Deserialize, Serialize)]
pub struct Session {
    pub id: String,
    pub object: String,
    pub agent: Agent,
    pub environment: Environment,
    pub created_at: i64,
    pub last_active_at: i64,
    pub status: SessionStatus,
    pub error: Option<String>,
    pub metadata: Metadata,
    pub required_actions: Vec<Unsupported>,
    pub usage: Option<Usage>,
    pub vault_ids: Vec<String>,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    InProgress,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub input_tokens_details: InputDetails,
    pub output_tokens_details: OutputDetails,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct InputDetails {
    pub cached_tokens: u64,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct OutputDetails {
    pub reasoning_tokens: u64,
}
impl Usage {
    pub fn from_turn(u: &TurnUsage) -> Option<Self> {
        if u.cost_status() == nanocodex::CostStatus::UsageNotReported {
            return None;
        }
        Some(Self {
            input_tokens: u.input_tokens(),
            output_tokens: u.output_tokens(),
            total_tokens: u.total_tokens(),
            input_tokens_details: InputDetails {
                cached_tokens: u.cached_input_tokens(),
            },
            output_tokens_details: OutputDetails {
                reasoning_tokens: u.reasoning_output_tokens(),
            },
        })
    }
    pub const fn add(&mut self, u: &Self) {
        self.input_tokens += u.input_tokens;
        self.output_tokens += u.output_tokens;
        self.total_tokens += u.total_tokens;
        self.input_tokens_details.cached_tokens += u.input_tokens_details.cached_tokens;
        self.output_tokens_details.reasoning_tokens += u.output_tokens_details.reasoning_tokens;
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Turn {
    pub id: String,
    pub object: String,
    pub agent_id: String,
    pub session_id: String,
    pub status: TurnStatus,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub subagent_id: Option<String>,
    pub error: Option<TurnError>,
    pub usage: Option<Usage>,
}
#[derive(Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    InProgress,
    Completed,
    Cancelled,
    Failed,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct TurnError {
    pub code: String,
    pub message: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Item {
    pub id: String,
    pub r#type: String,
    pub turn_id: String,
    pub role: String,
    pub status: String,
    pub phase: Option<String>,
    pub content: Vec<TextContent>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct TextContent {
    pub r#type: String,
    pub text: String,
}
impl Identified for Session {
    fn id(&self) -> &str {
        &self.id
    }
}
impl Identified for Turn {
    fn id(&self) -> &str {
        &self.id
    }
}
impl Identified for Item {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone, Serialize)]
pub struct Event {
    pub event_id: String,
    #[serde(flatten)]
    pub data: EventData,
}
#[derive(Clone, Serialize)]
#[serde(tag = "type")]
pub enum EventData {
    #[serde(rename = "agent.session.created")]
    Created { session: Session },
    #[serde(rename = "agent.session.idle")]
    Idle { session: Session },
    #[serde(rename = "agent.session.in_progress")]
    InProgress { session: Session },
    #[serde(rename = "agent.session.turn.created")]
    TurnCreated {
        session_id: String,
        turn_id: String,
        turn: Turn,
    },
    #[serde(rename = "agent.session.turn.in_progress")]
    TurnInProgress {
        session_id: String,
        turn_id: String,
        turn: Turn,
    },
    #[serde(rename = "agent.session.turn.completed")]
    Completed {
        session_id: String,
        turn_id: String,
        turn: Turn,
        usage: Option<Usage>,
    },
    #[serde(rename = "agent.session.turn.failed")]
    Failed {
        session_id: String,
        turn_id: String,
        turn: Turn,
        usage: Option<Usage>,
    },
    #[serde(rename = "agent.session.turn.cancelled")]
    Cancelled {
        session_id: String,
        turn_id: String,
        turn: Turn,
        usage: Option<Usage>,
    },
    #[serde(rename = "agent.session.turn.item.added")]
    ItemAdded {
        session_id: String,
        turn_id: String,
        item: Item,
        output_index: Option<usize>,
    },
    #[serde(rename = "agent.session.turn.item.done")]
    ItemDone {
        session_id: String,
        turn_id: String,
        item: Item,
        output_index: usize,
    },
    #[serde(rename = "agent.session.turn.output_text.delta")]
    Delta {
        session_id: String,
        turn_id: String,
        item_id: String,
        output_index: usize,
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "agent.session.turn.output_text.done")]
    TextDone {
        session_id: String,
        turn_id: String,
        item_id: String,
        output_index: usize,
        content_index: usize,
        text: String,
    },
}
