use crate::{
    App, LiveSession, SharedSession, id, now,
    protocol::*,
    store::{Record, Store},
};
use nanocodex::{
    Nanocodex, NanocodexError, OpenAi, Tools,
    agent::ExecutionEnvironment,
    oai::{
        events::{AgentEventData, AssistantEvent, RunEvent, ToolEvent},
        transport::ResponsesTransport,
    },
    tools::{
        Tool, ToolContext, ToolDefinition, ToolExposure, ToolInput, ToolOutput, ToolResult,
        contract::async_trait,
        mcp::{Mcp, McpServer},
    },
};
use nanocodex_subagents::{AgentStatus, AgentUpdate, SubagentControl};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, oneshot};

pub struct PendingCall {
    pub turn_id: String,
    pub item_id: String,
    pub sender: oneshot::Sender<ToolOutput>,
}
pub struct SessionRuntime {
    pub agent: Nanocodex,
    pub control: SubagentControl,
    updates: tokio::task::JoinHandle<()>,
}
impl SessionRuntime {
    pub async fn close(self) {
        let _ = self
            .control
            .close_all(&self.agent.session_id().to_string())
            .await;
        let _ = self.agent.shutdown().await;
        self.updates.abort();
        let _ = self.updates.await;
    }
}
struct ClientFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
    state: Weak<Mutex<LiveSession>>,
    store: Arc<Store>,
}
#[async_trait]
impl Tool for ClientFunction {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name.as_str(),
            self.description.as_str(),
            self.parameters.clone(),
        )
    }
    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let arguments: serde_json::Value = input.decode_json()?;
        if !jsonschema::is_valid(&self.parameters, &arguments) {
            return Ok(ToolOutput::error(
                "Arguments do not match the function schema",
            ));
        }
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| std::io::Error::other("Session closed"))?;
        let (send, receive) = oneshot::channel();
        {
            let mut state = state.lock().await;
            let turn_id = state
                .record
                .turns
                .iter()
                .rev()
                .find(|t| {
                    t.subagent_id.is_none()
                        && matches!(t.status, TurnStatus::InProgress | TurnStatus::Waiting)
                })
                .map(|t| t.id.clone())
                .ok_or_else(|| std::io::Error::other("No active turn"))?;
            let call_id = context.call_id().to_owned();
            if state.pending.contains_key(&call_id) {
                return Ok(ToolOutput::error("Duplicate active call ID"));
            }
            let item = Item::Function(FunctionItem {
                id: id("fc"),
                r#type: "function_call".into(),
                call_id: call_id.clone(),
                name: self.name.clone(),
                arguments: arguments.clone(),
                status: "in_progress".into(),
                turn_id: turn_id.clone(),
            });
            let item_id = item.id().to_owned();
            let mut next = state.record.clone();
            next.session.required_actions.push(RequiredAction {
                r#type: "function_call".into(),
                call_id: call_id.clone(),
                name: self.name.clone(),
                arguments,
                turn_id: turn_id.clone(),
            });
            next.session.status = SessionStatus::RequiresAction;
            if let Some(turn) = next.turns.iter_mut().find(|t| t.id == turn_id) {
                turn.status = TurnStatus::Waiting;
            }
            next.items.push(item.clone());
            self.store
                .save(&next)
                .map_err(|_| std::io::Error::other("Cannot store function call"))?;
            state.record = next;
            state.pending.insert(
                call_id,
                PendingCall {
                    turn_id: turn_id.clone(),
                    item_id,
                    sender: send,
                },
            );
            let session = state.record.session.clone();
            state.emit(EventData::ItemAdded {
                session_id: session.id.clone(),
                turn_id,
                output_index: state.output_index(&item),
                item,
            });
            state.emit(EventData::RequiresAction { session });
        }
        receive
            .await
            .map_err(|_| std::io::Error::other("Function call was cancelled").into())
    }
}

impl App {
    pub fn validate_tools(&self, tools: &[ToolConfig]) -> ApiResult<()> {
        for tool in tools {
            if let ToolConfig::Mcp {
                transport,
                connection_origin,
                ..
            } = tool
            {
                if connection_origin.as_deref().is_some_and(|s| s != "service") {
                    return Err(ApiError::invalid(
                        "environment MCP connections require an execution environment",
                    ));
                }
                let permitted = match transport {
                    McpTransport::Http { server_url, .. } => self.mcp_urls.contains(server_url),
                    McpTransport::Stdio { command, .. } => self.mcp_commands.contains(command),
                };
                if !permitted {
                    return Err(ApiError::invalid(
                        "MCP target is not in the operator allowlist",
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn runtime(
        &self,
        live: &SharedSession,
        record: &Record,
    ) -> Result<SessionRuntime, NanocodexError> {
        let config = &record.session.agent;
        let model = config
            .model
            .parse()
            .map_err(NanocodexError::InvalidRequest)?;
        let thinking = config
            .reasoning
            .effort
            .parse()
            .map_err(NanocodexError::InvalidRequest)?;
        let openai = OpenAi::builder(self.model_key.clone())
            .api_base_url(&self.model_url)
            .transport(ResponsesTransport::Https)
            .model(model)
            .thinking(thinking)
            .build()
            .map_err(|_| NanocodexError::InvalidRequest("Invalid model configuration".into()))?;
        let enabled = config.multi_agent.enabled;
        let (registry, control, mut updates) = nanocodex_subagents::channel(
            config.multi_agent.max_concurrent_subagents.unwrap_or(6) as usize,
        );
        let tools = record.tool_config.clone();
        let owner = record.session.agent.id.clone();
        let state = Arc::downgrade(live);
        let store = self.store.clone();
        let root_initialized = AtomicBool::new(false);
        let mut builder = Nanocodex::builder(openai)
            .instructions(crate::memory::instructions(config.instructions.as_deref()))
            .execution_environment(ExecutionEnvironment::new(
                chrono::Utc::now().format("%Y-%m-%d").to_string(),
                "Etc/UTC",
            ))
            .tools_factory(move |handle| {
                let root = !root_initialized.swap(true, Ordering::SeqCst);
                let mut builder = Tools::builder()
                    .without_defaults()
                    .exposure(ToolExposure::DirectAndCodeMode);
                for tool in crate::memory::tools(&owner, &store, &state, root) {
                    builder = builder.tool(tool);
                }
                let mut mcp = Mcp::builder();
                let mut has_mcp = false;
                for tool in &tools {
                    match tool {
                        ToolConfig::Function {
                            name,
                            description,
                            parameters,
                            ..
                        } if root => {
                            builder = builder.tool_with_exposure(
                                ClientFunction {
                                    name: name.clone(),
                                    description: description.clone(),
                                    parameters: parameters.clone(),
                                    state: state.clone(),
                                    store: store.clone(),
                                },
                                ToolExposure::DirectOnly,
                            );
                        }
                        ToolConfig::Mcp {
                            server_label,
                            transport,
                            allowed_tools,
                            ..
                        } => {
                            let mut server = match transport {
                                McpTransport::Http {
                                    server_url,
                                    authorization,
                                    headers,
                                } => {
                                    let mut server = McpServer::http(server_url);
                                    if let Some(value) = authorization {
                                        server = server.header("Authorization", value);
                                    }
                                    for (name, value) in headers.iter().flatten() {
                                        server = server.header(name, value);
                                    }
                                    server
                                }
                                McpTransport::Stdio { command, args, env } => {
                                    let mut server =
                                        McpServer::stdio(command).args(args.iter().cloned());
                                    for (name, value) in env.iter().flatten() {
                                        server = server.env(name, value);
                                    }
                                    server
                                }
                            };
                            if let Some(allowed) = allowed_tools {
                                server = server.enabled_tools(allowed.iter().cloned());
                            }
                            mcp = mcp.server(server_label, server);
                            has_mcp = true;
                        }
                        _ => {}
                    }
                }
                // MCP construction checks target syntax; admission already checked the operator allowlist.
                if has_mcp {
                    builder = builder.provider(
                        mcp.build()
                            .map_err(nanocodex::tools::ToolsBuildError::from)?,
                    );
                }
                let tools = builder.build()?;
                if enabled {
                    nanocodex_subagents::install_tools(tools, handle, registry.clone())
                } else {
                    Ok(tools)
                }
            });
        if let Some(snapshot) = &record.snapshot {
            builder = builder.resume(snapshot.clone());
        }
        let (agent, events) = builder.build()?;
        drop(events);
        let state = Arc::downgrade(live);
        let store = self.store.clone();
        let session_id = record.session.id.clone();
        let root_id = record.session.agent.id.clone();
        let updates = tokio::spawn(async move {
            let mut native_ids = BTreeMap::new();
            let mut active_turns = BTreeMap::new();
            while let Some(update) = updates.recv().await {
                let Some(live) = state.upgrade() else {
                    break;
                };
                let mut live = live.lock().await;
                if live.deleted {
                    break;
                }
                match update.update {
                    AgentUpdate::Added(child) => {
                        let child_id = subagent_id(&session_id, child.id);
                        let parent = child
                            .parent
                            .and_then(|p| native_ids.get(&p).cloned())
                            .unwrap_or_else(|| root_id.clone());
                        native_ids.insert(child.id, child_id.clone());
                        let subagent = Subagent {
                            id: child_id.clone(),
                            object: "agent.session.subagent".into(),
                            session_id: session_id.clone(),
                            parent_agent_id: parent,
                            name: Some(child.role),
                            instructions: Some(vec![TextContent {
                                r#type: "output_text".into(),
                                text: child.task,
                            }]),
                            opened_at: now(),
                            closed_at: None,
                            status: "active".into(),
                        };
                        live.record.subagents.push(subagent.clone());
                        live.emit(EventData::SubagentCreated { subagent });
                    }
                    AgentUpdate::Event {
                        id: native_id,
                        event,
                    } => {
                        let Some(child_id) = native_ids.get(&native_id) else {
                            continue;
                        };
                        let Ok(data) = event.data() else {
                            continue;
                        };
                        if matches!(data, AgentEventData::Run(RunEvent::Started(_))) {
                            let turn = Turn {
                                id: id("turn"),
                                object: "agent.session.turn".into(),
                                agent_id: child_id.clone(),
                                session_id: session_id.clone(),
                                status: TurnStatus::InProgress,
                                created_at: now(),
                                started_at: Some(now()),
                                completed_at: None,
                                subagent_id: Some(child_id.clone()),
                                error: None,
                                usage: None,
                            };
                            active_turns.insert(native_id, turn.id.clone());
                            live.record.turns.push(turn.clone());
                            live.emit(EventData::TurnCreated {
                                session_id: session_id.clone(),
                                turn_id: turn.id.clone(),
                                turn: turn.clone(),
                            });
                            live.emit(EventData::TurnInProgress {
                                session_id: session_id.clone(),
                                turn_id: turn.id.clone(),
                                turn,
                            });
                        }
                        if let Some(turn_id) = active_turns.get(&native_id) {
                            live.observe(turn_id, data);
                        }
                    }
                    AgentUpdate::Status {
                        id: native_id,
                        status,
                    } => {
                        if let Some(turn_id) = active_turns.get(&native_id) {
                            let terminal = match &status {
                                AgentStatus::Completed { .. } => Some(TurnStatus::Completed),
                                AgentStatus::Failed { .. } => Some(TurnStatus::Failed),
                                AgentStatus::Interrupted => Some(TurnStatus::Cancelled),
                                _ => None,
                            };
                            if let Some(status) = terminal {
                                live.terminal_child(turn_id, status);
                            }
                        }
                        if matches!(status, AgentStatus::Closed)
                            && let Some(child) = live
                                .record
                                .subagents
                                .iter_mut()
                                .find(|s| native_ids.get(&native_id) == Some(&s.id))
                        {
                            child.status = "closed".into();
                            child.closed_at = Some(now());
                            let subagent = child.clone();
                            live.emit(EventData::SubagentClosed { subagent });
                        }
                    }
                    AgentUpdate::Message(_) => {}
                }
                if store.save(&live.record).is_err() {
                    eprintln!("Cannot save subagent state");
                    std::process::exit(1);
                }
            }
        });
        Ok(SessionRuntime {
            agent,
            control,
            updates,
        })
    }
}

impl LiveSession {
    pub fn terminal_child(&mut self, turn_id: &str, status: TurnStatus) {
        if let Some(turn) = self.record.turns.iter_mut().find(|t| t.id == turn_id) {
            if !matches!(turn.status, TurnStatus::InProgress | TurnStatus::Waiting) {
                return;
            }
            turn.status = status;
            turn.completed_at = Some(now());
            let turn = turn.clone();
            let session_id = self.record.session.id.clone();
            let turn_id = turn.id.clone();
            self.emit(match status {
                TurnStatus::Completed => EventData::Completed {
                    session_id,
                    turn_id,
                    usage: turn.usage.clone(),
                    turn,
                },
                TurnStatus::Cancelled => EventData::Cancelled {
                    session_id,
                    turn_id,
                    usage: turn.usage.clone(),
                    turn,
                },
                _ => EventData::Failed {
                    session_id,
                    turn_id,
                    usage: turn.usage.clone(),
                    turn,
                },
            });
        }
    }
    pub fn observe(&mut self, turn_id: &str, data: AgentEventData) {
        let session_id = self.record.session.id.clone();
        match data {
            AgentEventData::Run(RunEvent::Completed(end) | RunEvent::Failed(end)) => {
                if let Some(turn) = self
                    .record
                    .turns
                    .iter_mut()
                    .find(|t| t.id == turn_id && t.subagent_id.is_some())
                    && end.cost_status != nanocodex::CostStatus::UsageNotReported
                {
                    let usage = end.metrics.usage;
                    turn.usage = Some(Usage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        total_tokens: usage.total_tokens,
                        input_tokens_details: InputDetails {
                            cached_tokens: usage.cached_input_tokens,
                        },
                        output_tokens_details: OutputDetails {
                            reasoning_tokens: usage.reasoning_output_tokens,
                        },
                    });
                }
            }
            AgentEventData::Assistant(event) => {
                let (key, text, done, phase) = match event {
                    AssistantEvent::Delta(d) => (
                        d.item_id
                            .unwrap_or_else(|| format!("call-{}", d.model_call_index)),
                        d.text,
                        false,
                        d.phase,
                    ),
                    AssistantEvent::Message(m) => (
                        m.item_id
                            .unwrap_or_else(|| format!("call-{}", m.model_call_index)),
                        m.text,
                        true,
                        m.phase,
                    ),
                    _ => return,
                };
                let item_id = format!("msg_{turn_id}_{key}");
                let index = self
                    .record
                    .items
                    .iter()
                    .position(|i| i.id() == item_id)
                    .unwrap_or_else(|| {
                        let item = Item::Message(MessageItem {
                            id: item_id.clone(),
                            r#type: "message".into(),
                            turn_id: turn_id.into(),
                            role: "assistant".into(),
                            status: "in_progress".into(),
                            phase: phase.map(|p| {
                                match p {
                                    nanocodex::oai::responses::MessagePhase::Commentary => {
                                        "commentary"
                                    }
                                    nanocodex::oai::responses::MessagePhase::FinalAnswer => {
                                        "final_answer"
                                    }
                                }
                                .into()
                            }),
                            content: vec![TextContent {
                                r#type: "output_text".into(),
                                text: String::new(),
                            }],
                        });
                        self.record.items.push(item.clone());
                        self.emit(EventData::ItemAdded {
                            session_id: session_id.clone(),
                            turn_id: turn_id.into(),
                            output_index: self.output_index(&item),
                            item,
                        });
                        self.record.items.len() - 1
                    });
                let output_index = self.output_index(&self.record.items[index]).unwrap_or(0);
                if let Item::Message(item) = &mut self.record.items[index] {
                    if done {
                        item.content[0].text = text.clone();
                        item.status = "completed".into();
                    } else {
                        item.content[0].text.push_str(&text);
                    }
                }
                if done {
                    self.emit(EventData::TextDone {
                        session_id: session_id.clone(),
                        turn_id: turn_id.into(),
                        item_id,
                        output_index,
                        content_index: 0,
                        text,
                    });
                    self.emit(EventData::ItemDone {
                        session_id,
                        turn_id: turn_id.into(),
                        item: self.record.items[index].clone(),
                        output_index,
                    });
                } else {
                    self.emit(EventData::Delta {
                        session_id,
                        turn_id: turn_id.into(),
                        item_id,
                        output_index,
                        content_index: 0,
                        delta: text,
                    });
                }
            }
            AgentEventData::Tool(ToolEvent::Call(call)) => {
                if self
                    .record
                    .tool_config
                    .iter()
                    .any(|t| matches!(t, ToolConfig::Function { name, .. } if name == &call.tool))
                {
                    return;
                }
                let Ok(arguments) = call.decode_arguments::<serde_json::Value>() else {
                    return;
                };
                let item = if let Some((label, name)) = call
                    .tool
                    .strip_prefix("mcp__")
                    .and_then(|s| s.split_once("__"))
                {
                    Some(Item::Mcp(McpItem {
                        id: call.call_id.clone(),
                        r#type: "mcp_call".into(),
                        server_label: label.into(),
                        name: name.into(),
                        arguments,
                        status: "in_progress".into(),
                        turn_id: turn_id.into(),
                        output: None,
                        error: None,
                    }))
                } else {
                    self.coordination(&call.tool, &arguments, turn_id)
                        .map(|action| {
                            Item::Coordination(CoordinationItem {
                                id: call.call_id.clone(),
                                status: "in_progress".into(),
                                turn_id: turn_id.into(),
                                action,
                            })
                        })
                };
                if let Some(item) = item {
                    self.record.items.push(item.clone());
                    self.emit(EventData::ItemAdded {
                        session_id,
                        turn_id: turn_id.into(),
                        output_index: self.output_index(&item),
                        item,
                    });
                }
            }
            AgentEventData::Tool(ToolEvent::Result(result)) => {
                let completed =
                    matches!(result.status, nanocodex::oai::events::ToolStatus::Completed);
                let item = self
                    .record
                    .items
                    .iter_mut()
                    .find(|i| i.id() == result.call_id && i.turn_id() == turn_id);
                let status = if completed {
                    "completed"
                } else if matches!(result.status, nanocodex::oai::events::ToolStatus::Cancelled) {
                    "incomplete"
                } else {
                    "failed"
                };
                if let Some(item) = item {
                    match item {
                        Item::Mcp(item) => {
                            item.status = status.into();
                            item.output = Some(result.structured_result);
                            if !completed {
                                item.error = Some("MCP tool failed".into());
                            }
                        }
                        Item::Coordination(item) => item.status = status.into(),
                        _ => return,
                    }
                    let item = item.clone();
                    self.emit(EventData::ItemDone {
                        session_id,
                        turn_id: turn_id.into(),
                        output_index: self.output_index(&item).unwrap_or(0),
                        item,
                    });
                }
            }
            _ => {}
        }
    }
}

fn subagent_id(session_id: &str, native_id: impl std::fmt::Display) -> String {
    format!("subagent_{session_id}_{native_id}")
}
impl LiveSession {
    pub fn output_index(&self, item: &Item) -> Option<usize> {
        self.record
            .items
            .iter()
            .filter(|i| {
                i.turn_id() == item.turn_id()
                    && !matches!(i, Item::Message(m) if m.role == "user")
                    && !matches!(i, Item::FunctionOutput(_))
            })
            .position(|i| i.id() == item.id())
    }
    fn coordination(
        &self,
        tool: &str,
        args: &serde_json::Value,
        turn_id: &str,
    ) -> Option<CoordinationAction> {
        let sender = self
            .record
            .turns
            .iter()
            .find(|t| t.id == turn_id)?
            .agent_id
            .clone();
        let target = || {
            args.get("agent_id")
                .and_then(serde_json::Value::as_u64)
                .map(|id| subagent_id(&self.record.session.id, id))
        };
        let content = |text: &str| {
            vec![TextContent {
                r#type: "input_text".into(),
                text: text.into(),
            }]
        };
        match tool {
            "spawn_agent" => Some(CoordinationAction::Create {
                agent_id: sender,
                content: content(args.get("task")?.as_str()?),
                model: args
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                reasoning_effort: args
                    .get("thinking")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            }),
            "send_agent_message" => Some(CoordinationAction::Send {
                sender_agent_id: sender,
                recipient_agent_id: target()?,
                content: content(args.get("message")?.as_str()?),
            }),
            "wait_agent" => Some(CoordinationAction::Wait {
                sender_agent_id: sender,
                recipient_agent_ids: args
                    .get("agent_ids")?
                    .as_array()?
                    .iter()
                    .map(|v| {
                        v.as_u64()
                            .map(|id| subagent_id(&self.record.session.id, id))
                    })
                    .collect::<Option<Vec<_>>>()?,
            }),
            "interrupt_agent" => Some(CoordinationAction::Interrupt {
                sender_agent_id: sender,
                recipient_agent_id: target()?,
            }),
            "close_agent" => Some(CoordinationAction::Close {
                sender_agent_id: sender,
                recipient_agent_id: target()?,
            }),
            _ => None,
        }
    }
}
