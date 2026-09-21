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
    pub agent: Option<AgentInput>,
    pub agent_id: Option<String>,
    pub input: Option<Input>,
    #[serde(default)]
    pub stream: bool,
    pub metadata: Option<Metadata>,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInput {
    pub model: Option<String>,
    pub instructions: Option<String>,
    pub tools: Option<Vec<ToolConfig>>,
    pub multi_agent: Option<MultiAgent>,
    pub reasoning: Option<Reasoning>,
    pub name: Option<String>,
    pub metadata: Option<Metadata>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ToolConfig {
    #[serde(rename = "function")]
    Function {
        name: String,
        description: String,
        parameters: serde_json::Value,
        #[serde(default)]
        defer_loading: bool,
    },
    #[serde(rename = "mcp")]
    Mcp {
        server_label: String,
        transport: McpTransport,
        allowed_tools: Option<Vec<String>>,
        connection_origin: Option<String>,
        #[serde(default)]
        required: bool,
    },
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum McpTransport {
    #[serde(rename = "http")]
    Http {
        server_url: String,
        authorization: Option<String>,
        headers: Option<Metadata>,
    },
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        env: Option<Metadata>,
    },
}
impl ToolConfig {
    pub fn public(&self) -> Self {
        let mut tool = self.clone();
        if let Self::Mcp { transport, .. } = &mut tool {
            match transport {
                McpTransport::Http {
                    authorization,
                    headers,
                    ..
                } => {
                    *authorization = None;
                    *headers = None;
                }
                McpTransport::Stdio { env, .. } => *env = None,
            }
        }
        tool
    }
}

impl AgentInput {
    pub fn validate(&self) -> ApiResult<Model> {
        if self.instructions.as_ref().is_some_and(|s| s.len() > 32_768) {
            return Err(ApiError::invalid("Instructions exceed 32768 bytes"));
        }
        let name = self.model.as_deref().unwrap_or("gpt-5.6-sol");
        let model: Model = name.parse().map_err(ApiError::invalid)?;
        if model.as_str() != name {
            return Err(ApiError::invalid("Use a full model ID"));
        }
        let mut names = std::collections::BTreeSet::new();
        for tool in self.tools.as_deref().unwrap_or_default() {
            if matches!(
                tool,
                ToolConfig::Function {
                    defer_loading: true,
                    ..
                }
            ) {
                return Err(ApiError::invalid(
                    "Deferred client functions are not supported",
                ));
            }
            if matches!(tool, ToolConfig::Mcp { required: true, .. }) {
                return Err(ApiError::invalid(
                    "Required MCP startup checks are not supported",
                ));
            }
            let name = match tool {
                ToolConfig::Function {
                    name, parameters, ..
                } => {
                    jsonschema::validator_for(parameters)
                        .map_err(|_| ApiError::invalid("Invalid function JSON schema"))?;
                    name
                }
                ToolConfig::Mcp { server_label, .. } => server_label,
            };
            if name.is_empty()
                || name.len() > 64
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || !names.insert(name)
                || [
                    "exec",
                    "wait",
                    "tool_search",
                    "spawn_agent",
                    "submit_result",
                    "send_agent_message",
                    "list_agents",
                    "wait_agent",
                    "interrupt_agent",
                    "close_agent",
                    "memory_read",
                    "memory_search",
                    "memory_journal",
                    "memory_write",
                    "memory_delete",
                ]
                .contains(&name.as_str())
            {
                return Err(ApiError::invalid(
                    "Tool names must be unique, valid, and not reserved",
                ));
            }
        }
        if self.tools.as_ref().is_some_and(|t| t.len() > 64) {
            return Err(ApiError::invalid("At most 64 tools are supported"));
        }
        if let Some(config) = &self.multi_agent
            && config
                .max_concurrent_subagents
                .is_some_and(|n| n == 0 || n > 32)
        {
            return Err(ApiError::invalid(
                "Subagent concurrency must be from 1 to 32",
            ));
        }
        if let Some(reasoning) = &self.reasoning {
            reasoning
                .effort
                .parse::<nanocodex::Thinking>()
                .map_err(ApiError::invalid)?;
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
    #[serde(rename = "agent.session.input.tool_result")]
    ToolResult {
        call_id: String,
        turn_id: String,
        success: bool,
        output: Option<FunctionOutput>,
        error: Option<String>,
    },
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum FunctionOutput {
    Text(String),
    Content(Vec<InputText>),
}
impl FunctionOutput {
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Content(c) => c.iter().map(InputText::text).collect::<Vec<_>>().join("\n"),
        }
    }
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
    pub tools: Vec<ToolConfig>,
    pub multi_agent: MultiAgent,
    pub reasoning: Reasoning,
    pub service_tier: String,
    pub text: TextConfig,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MultiAgent {
    pub enabled: bool,
    pub max_concurrent_subagents: Option<u32>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
    pub required_actions: Vec<RequiredAction>,
    pub usage: Option<Usage>,
    pub vault_ids: Vec<String>,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    InProgress,
    RequiresAction,
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
    Waiting,
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
pub struct MessageItem {
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
#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Item {
    Message(MessageItem),
    Function(FunctionItem),
    FunctionOutput(FunctionOutputItem),
    Mcp(McpItem),
    Coordination(CoordinationItem),
}
impl Identified for Item {
    fn id(&self) -> &str {
        match self {
            Self::Message(v) => &v.id,
            Self::Function(v) => &v.id,
            Self::FunctionOutput(v) => &v.id,
            Self::Mcp(v) => &v.id,
            Self::Coordination(v) => &v.id,
        }
    }
}
impl Item {
    pub fn turn_id(&self) -> &str {
        match self {
            Self::Message(v) => &v.turn_id,
            Self::Function(v) => &v.turn_id,
            Self::FunctionOutput(v) => &v.turn_id,
            Self::Mcp(v) => &v.turn_id,
            Self::Coordination(v) => &v.turn_id,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct FunctionItem {
    pub id: String,
    pub r#type: String,
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub status: String,
    pub turn_id: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct FunctionOutputItem {
    pub id: String,
    pub r#type: String,
    pub call_id: String,
    pub output: Option<FunctionOutput>,
    pub error: Option<String>,
    pub status: String,
    pub turn_id: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct McpItem {
    pub id: String,
    pub r#type: String,
    pub server_label: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub status: String,
    pub turn_id: String,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct CoordinationItem {
    pub id: String,
    pub status: String,
    pub turn_id: String,
    #[serde(flatten)]
    pub action: CoordinationAction,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum CoordinationAction {
    #[serde(rename = "create_subagent_call")]
    Create {
        agent_id: String,
        content: Vec<TextContent>,
        model: Option<String>,
        reasoning_effort: Option<String>,
    },
    #[serde(rename = "send_subagent_input_call")]
    Send {
        recipient_agent_id: String,
        sender_agent_id: String,
        content: Vec<TextContent>,
    },
    #[serde(rename = "wait_for_subagents_call")]
    Wait {
        recipient_agent_ids: Vec<String>,
        sender_agent_id: String,
    },
    #[serde(rename = "interrupt_subagent_call")]
    Interrupt {
        recipient_agent_id: String,
        sender_agent_id: String,
    },
    #[serde(rename = "close_subagent_call")]
    Close {
        recipient_agent_id: String,
        sender_agent_id: String,
    },
}
#[derive(Clone, Deserialize, Serialize)]
pub struct RequiredAction {
    pub r#type: String,
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub turn_id: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Subagent {
    pub id: String,
    pub object: String,
    pub session_id: String,
    pub parent_agent_id: String,
    pub name: Option<String>,
    pub instructions: Option<Vec<TextContent>>,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub status: String,
}
impl Identified for Subagent {
    fn id(&self) -> &str {
        &self.id
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct SavedAgent {
    #[serde(flatten)]
    pub agent: Agent,
    pub object: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub metadata: Metadata,
}
impl Identified for SavedAgent {
    fn id(&self) -> &str {
        &self.agent.id
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
    #[serde(rename = "agent.session.requires_action")]
    RequiresAction { session: Session },
    #[serde(rename = "agent.session.subagent.created")]
    SubagentCreated { subagent: Subagent },
    #[serde(rename = "agent.session.subagent.closed")]
    SubagentClosed { subagent: Subagent },
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
