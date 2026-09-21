mod protocol;
mod store;

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event as SseEvent, KeepAlive},
    },
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use nanocodex::{
    Model, Nanocodex, NanocodexError, OpenAi, Thinking, Tools,
    agent::ExecutionEnvironment,
    oai::{
        Prompt, PromptMessage,
        events::{AgentEventData, AssistantEvent},
        transport::ResponsesTransport,
    },
};
use protocol::*;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};
use store::{Record, Store};
use tokio::sync::{Mutex, RwLock, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::now_v7().simple())
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
struct LiveSession {
    record: Record,
    events: broadcast::Sender<Event>,
    active: Option<CancellationToken>,
    deleted: bool,
}
impl LiveSession {
    fn new(record: Record) -> Self {
        Self {
            record,
            events: broadcast::channel(256).0,
            active: None,
            deleted: false,
        }
    }
    fn emit(&self, data: EventData) {
        let _ = self.events.send(Event {
            event_id: id("evt"),
            data,
        });
    }
    fn session(&self) -> ApiResult<Session> {
        if self.deleted {
            return Err(ApiError::missing());
        }
        Ok(self.record.session.clone())
    }
}
type SharedSession = Arc<Mutex<LiveSession>>;
struct App {
    store: Store,
    sessions: RwLock<BTreeMap<String, SharedSession>>,
    slots: Arc<Semaphore>,
    api_token_hash: [u8; 32],
    model_key: String,
    model_url: String,
    shutdown: CancellationToken,
}
impl App {
    async fn session(&self, id: &str) -> ApiResult<SharedSession> {
        self.sessions
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(ApiError::missing)
    }

    async fn start(
        self: &Arc<Self>,
        live: &SharedSession,
        messages: Vec<InputMessage>,
        key: Option<(String, [u8; 32])>,
    ) -> ApiResult<()> {
        let mut state = live.lock().await;
        state.session()?;
        if let Some((key, digest)) = &key
            && let Some(previous) = state.record.submissions.get(key)
        {
            return if previous == digest {
                Ok(())
            } else {
                Err(ApiError::conflict(
                    "Idempotency-Key was used with a different body",
                ))
            };
        }
        if state.active.is_some() {
            return Err(ApiError::conflict(
                "active_turn_not_steerable: wait for the current turn to finish",
            ));
        }
        if self.shutdown.is_cancelled() {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "Service is stopping".into(),
            ));
        }
        if state.record.turns.len() >= 200 {
            return Err(ApiError::invalid(
                "Session limit is 200 turns; create a new session",
            ));
        }
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "All agent slots are in use; try again later".into(),
            )
        })?;
        let session_id = state.record.session.id.clone();
        let turn = Turn {
            id: id("turn"),
            object: "agent.session.turn".into(),
            agent_id: state.record.session.agent.id.clone(),
            session_id: session_id.clone(),
            status: TurnStatus::InProgress,
            created_at: now(),
            started_at: Some(now()),
            completed_at: None,
            subagent_id: None,
            error: None,
            usage: None,
        };
        let turn_id = turn.id.clone();
        let mut record = state.record.clone();
        let user_items: Vec<Item> = messages
            .iter()
            .map(|m| Item {
                id: id("msg"),
                r#type: "message".into(),
                turn_id: turn_id.clone(),
                role: "user".into(),
                status: "completed".into(),
                phase: None,
                content: m
                    .content
                    .iter()
                    .map(|c| TextContent {
                        r#type: "input_text".into(),
                        text: c.text().into(),
                    })
                    .collect(),
            })
            .collect();
        record.items.extend(user_items.iter().cloned());
        record.turns.push(turn.clone());
        record.session.status = SessionStatus::InProgress;
        record.session.last_active_at = now();
        if let Some((key, digest)) = key {
            record.submissions.insert(key, digest);
        }
        self.store.save(&record)?;
        state.record = record;
        let cancel = self.shutdown.child_token();
        state.active = Some(cancel.clone());
        state.emit(EventData::InProgress {
            session: state.record.session.clone(),
        });
        state.emit(EventData::TurnCreated {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            turn: turn.clone(),
        });
        state.emit(EventData::TurnInProgress {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            turn,
        });
        for item in user_items {
            state.emit(EventData::ItemAdded {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                item,
                output_index: None,
            });
        }
        let record = state.record.clone();
        let app = self.clone();
        let live = live.clone();
        tokio::spawn(async move {
            let _permit = permit;
            app.run(live, record, turn_id, messages, cancel).await;
        });
        Ok(())
    }

    async fn run(
        &self,
        live: SharedSession,
        record: Record,
        turn_id: String,
        messages: Vec<InputMessage>,
        cancel: CancellationToken,
    ) {
        let session_id = record.session.id.clone();
        let item_id = id("msg");
        let mut output = Item {
            id: item_id.clone(),
            r#type: "message".into(),
            turn_id: turn_id.clone(),
            role: "assistant".into(),
            status: "in_progress".into(),
            phase: Some("final_answer".into()),
            content: vec![TextContent {
                r#type: "output_text".into(),
                text: String::new(),
            }],
        };
        live.lock().await.emit(EventData::ItemAdded {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            item: output.clone(),
            output_index: Some(0),
        });
        let result = async {
            let model: Model = record.session.agent.model.parse().map_err(NanocodexError::InvalidRequest)?;
            let openai = OpenAi::builder(self.model_key.clone()).api_base_url(&self.model_url).transport(ResponsesTransport::Https).model(model).thinking(Thinking::Low).build().map_err(|_| NanocodexError::InvalidRequest("Model client configuration is invalid".into()))?;
            let mut builder = Nanocodex::builder(openai)
                .instructions(record.session.agent.instructions.clone().unwrap_or_default())
                .tools(Tools::builder().without_defaults().build()?)
                .execution_environment(ExecutionEnvironment::new(chrono::Utc::now().format("%Y-%m-%d").to_string(), "Etc/UTC"));
            if let Some(snapshot) = record.snapshot { builder = builder.resume(snapshot); }
            let (agent, events) = builder.build()?;
            drop(events);
            let answer = async {
                let mut texts: Vec<String> = messages.iter().map(InputMessage::text).collect();
                let last = texts.pop().ok_or_else(|| NanocodexError::InvalidRequest("Empty input".into()))?;
                let prompt = Prompt::new(last).with_transcript(texts.into_iter().map(PromptMessage::user));
                let mut turn = agent.prompt(prompt).await?;
                let timeout = tokio::time::sleep(Duration::from_secs(300));
                tokio::pin!(timeout);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => { let _ = turn.cancel().await; break; }
                        _ = &mut timeout => { let _ = turn.cancel().await; return Err(NanocodexError::InvalidRequest("Turn time limit reached".into())); }
                        event = turn.next() => {
                            let Some(event) = event else { break; };
                            if let Ok(AgentEventData::Assistant(AssistantEvent::Delta(delta))) = event.data() {
                                // The none environment has no tool continuation. Only public assistant text leaves the engine.
                                output.content[0].text.push_str(&delta.text);
                                live.lock().await.emit(EventData::Delta { session_id: session_id.clone(), turn_id: turn_id.clone(), item_id: item_id.clone(), output_index: 0, content_index: 0, delta: delta.text });
                            }
                        }
                    }
                }
                turn.await
            }.await;
            let _ = agent.shutdown().await;
            answer
        }.await;
        let mut state = live.lock().await;
        let mut next = state.record.clone();
        let Some(turn) = next.turns.iter_mut().find(|t| t.id == turn_id) else {
            return;
        };
        turn.completed_at = Some(now());
        match result {
            Ok(result) => {
                turn.status = TurnStatus::Completed;
                turn.usage = Usage::from_turn(result.usage());
                if let Some(usage) = &turn.usage {
                    if let Some(total) = &mut next.session.usage {
                        total.add(usage);
                    } else {
                        next.session.usage = Some(usage.clone());
                    }
                }
                output.content[0].text = result.final_message().into();
                output.status = "completed".into();
                next.snapshot = Some(result.snapshot());
            }
            Err(NanocodexError::TurnCancelled) => {
                turn.status = TurnStatus::Cancelled;
                output.status = "incomplete".into();
            }
            Err(_) => {
                turn.status = TurnStatus::Failed;
                turn.error = Some(TurnError {
                    code: "server_error".into(),
                    message: "The model request failed or reached its time limit".into(),
                });
                output.status = "incomplete".into();
            }
        }
        let turn = turn.clone();
        next.items.push(output.clone());
        next.session.status = SessionStatus::Idle;
        next.session.last_active_at = now();
        // Do not publish completion until history and the engine checkpoint commit together.
        if self.store.save(&next).is_err() {
            eprintln!("Could not save a terminal turn; stopping for recovery");
            std::process::exit(1);
        }
        state.record = next;
        state.active = None;
        state.emit(EventData::TextDone {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            item_id,
            output_index: 0,
            content_index: 0,
            text: output.content[0].text.clone(),
        });
        state.emit(EventData::ItemDone {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            item: output,
            output_index: 0,
        });
        let usage = turn.usage.clone();
        state.emit(match turn.status {
            TurnStatus::Completed => EventData::Completed {
                session_id,
                turn_id,
                turn,
                usage,
            },
            TurnStatus::Cancelled => EventData::Cancelled {
                session_id,
                turn_id,
                turn,
                usage,
            },
            _ => EventData::Failed {
                session_id,
                turn_id,
                turn,
                usage,
            },
        });
        state.emit(EventData::Idle {
            session: state.record.session.clone(),
        });
    }
}

async fn authenticate(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let valid = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .is_some_and(|token| {
            let hash = Sha256::digest(token.as_bytes());
            hash.iter()
                .zip(app.api_token_hash)
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                == 0
        });
    if !valid {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "A valid API bearer token is required".into(),
        )
        .into_response();
    }
    next.run(request).await
}
fn body<T>(body: Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    body.map(|Json(v)| v)
        .map_err(|e| ApiError::invalid(e.body_text()))
}
fn query(query: Result<Query<PageQuery>, QueryRejection>) -> ApiResult<PageQuery> {
    query
        .map(|Query(v)| v)
        .map_err(|e| ApiError::invalid(e.body_text()))
}

fn event_stream(
    receiver: broadcast::Receiver<Event>,
    initial: Event,
    until_idle: bool,
    shutdown: CancellationToken,
) -> Response {
    let events = stream::once(async move {
        Ok::<_, Infallible>(SseEvent::default().json_data(initial).unwrap_or_default())
    })
    .chain(stream::unfold(
        (receiver, false, shutdown),
        move |(mut receiver, ended, shutdown)| async move {
            if ended {
                return None;
            }
            let received = tokio::select! {
                _ = shutdown.cancelled() => return None,
                event = receiver.recv() => event,
            };
            match received {
                Ok(event) => {
                    let ended = until_idle && matches!(event.data, EventData::Idle { .. });
                    let sse = SseEvent::default()
                        .id(&event.event_id)
                        .json_data(&event)
                        .ok()?;
                    Some((Ok(sse), (receiver, ended, shutdown)))
                }
                // Slow consumers must reconnect and fetch items/turns. Never hide a gap.
                Err(_) => None,
            }
        },
    ));
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
        .into_response()
}

async fn create(
    State(app): State<Arc<App>>,
    request: Result<Json<CreateSession>, JsonRejection>,
) -> ApiResult<Response> {
    let request = body(request)?;
    request.agent.validate()?;
    let messages = request.input.map(Input::messages).transpose()?;
    if request.stream && messages.is_none() {
        return Err(ApiError::invalid("stream:true requires input"));
    }
    let session = Session {
        id: id("sess"),
        object: "agent.session".into(),
        agent: Agent {
            id: id("agent"),
            model: request.agent.model,
            instructions: request.agent.instructions,
            name: None,
            tools: vec![],
            multi_agent: MultiAgent {
                enabled: false,
                max_concurrent_subagents: None,
            },
            reasoning: Reasoning {
                effort: "low".into(),
                summary: None,
            },
            service_tier: "default".into(),
            text: TextConfig {
                format: TextFormat {
                    r#type: "text".into(),
                },
                verbosity: "medium".into(),
            },
        },
        environment: request.environment,
        created_at: now(),
        last_active_at: now(),
        status: SessionStatus::Idle,
        error: None,
        metadata: metadata(request.metadata)?,
        required_actions: vec![],
        usage: None,
        vault_ids: vec![],
    };
    let record = Record {
        session: session.clone(),
        turns: vec![],
        items: vec![],
        snapshot: None,
        submissions: BTreeMap::new(),
    };
    let live = Arc::new(Mutex::new(LiveSession::new(record.clone())));
    let receiver = live.lock().await.events.subscribe();
    {
        let mut sessions = app.sessions.write().await;
        if sessions.len() >= 1000 {
            return Err(ApiError::invalid(
                "Service limit is 1000 sessions; delete unused sessions",
            ));
        }
        app.store.save(&record)?;
        sessions.insert(session.id.clone(), live.clone());
    }
    if let Some(messages) = messages
        && let Err(error) = app.start(&live, messages, None).await
    {
        app.store.delete(&session.id)?;
        app.sessions.write().await.remove(&session.id);
        return Err(error);
    }
    if request.stream {
        Ok(event_stream(
            receiver,
            Event {
                event_id: id("evt"),
                data: EventData::Created { session },
            },
            true,
            app.shutdown.clone(),
        ))
    } else {
        Ok(Json(live.lock().await.session()?).into_response())
    }
}
async fn retrieve(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Session>> {
    Ok(Json(app.session(&id).await?.lock().await.session()?))
}
async fn update(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    request: Result<Json<UpdateSession>, JsonRejection>,
) -> ApiResult<Json<Session>> {
    let request = body(request)?;
    let live = app.session(&id).await?;
    let mut state = live.lock().await;
    state.session()?;
    if request.metadata.is_some() {
        let mut next = state.record.clone();
        next.session.metadata = metadata(request.metadata)?;
        app.store.save(&next)?;
        state.record = next;
    }
    Ok(Json(state.record.session.clone()))
}
async fn list(
    State(app): State<Arc<App>>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Session>>> {
    let params = query(params)?;
    let mut sessions = vec![];
    for live in app.sessions.read().await.values() {
        sessions.push(live.lock().await.session()?);
    }
    Ok(Json(params.page(sessions)?))
}
#[derive(serde::Serialize)]
struct Deleted {
    id: String,
    object: &'static str,
    deleted: bool,
}
async fn delete(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Deleted>> {
    let mut sessions = app.sessions.write().await;
    let live = sessions.get(&id).ok_or_else(ApiError::missing)?.clone();
    let mut state = live.lock().await;
    if state.active.is_some() {
        return Err(ApiError::conflict(
            "Cancel the active turn and wait for idle before deletion",
        ));
    }
    app.store.delete(&id)?;
    state.deleted = true;
    sessions.remove(&id);
    Ok(Json(Deleted {
        id,
        object: "agent.session.deleted",
        deleted: true,
    }))
}
async fn items(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Item>>> {
    let params = query(params)?;
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    Ok(Json(params.page(state.record.items.clone())?))
}
async fn turns(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Turn>>> {
    let params = query(params)?;
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    Ok(Json(params.page(state.record.turns.clone())?))
}
async fn turn(
    State(app): State<Arc<App>>,
    Path((id, turn_id)): Path<(String, String)>,
) -> ApiResult<Json<Turn>> {
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    Ok(Json(
        state
            .record
            .turns
            .iter()
            .find(|t| t.id == turn_id)
            .cloned()
            .ok_or_else(ApiError::missing)?,
    ))
}
async fn subscribe(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Response> {
    let live = app.session(&id).await?;
    let state = live.lock().await;
    let session = state.session()?;
    let initial = match session.status {
        SessionStatus::Idle => EventData::Idle { session },
        SessionStatus::InProgress => EventData::InProgress { session },
    };
    Ok(event_stream(
        state.events.subscribe(),
        Event {
            event_id: id_event(),
            data: initial,
        },
        false,
        app.shutdown.clone(),
    ))
}
fn id_event() -> String {
    id("evt")
}
async fn submit(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<SubmitEvents>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let mut request = body(request)?;
    if request.events.len() != 1 {
        return Err(ApiError::invalid("Submit exactly one event per request"));
    }
    let key = if let Some(header) = headers.get("idempotency-key") {
        let key = header
            .to_str()
            .map_err(|_| ApiError::invalid("Invalid Idempotency-Key"))?;
        if key.is_empty() || key.len() > 256 {
            return Err(ApiError::invalid(
                "Idempotency-Key must have 1 to 256 bytes",
            ));
        }
        let bytes = serde_json::to_vec(&request).map_err(|_| ApiError::internal())?;
        Some((key.to_owned(), Sha256::digest(bytes).into()))
    } else {
        None
    };
    let live = app.session(&id).await?;
    match request.events.remove(0) {
        InputEvent::Message { input } => {
            app.start(&live, Input::Messages(input).messages()?, key)
                .await?;
        }
        InputEvent::Cancel => {
            let mut state = live.lock().await;
            state.session()?;
            if let Some((key, digest)) = key {
                if let Some(previous) = state.record.submissions.get(&key) {
                    return if previous == &digest {
                        Ok(StatusCode::NO_CONTENT)
                    } else {
                        Err(ApiError::conflict(
                            "Idempotency-Key was used with a different body",
                        ))
                    };
                }
                if state.record.submissions.len() >= 400 {
                    return Err(ApiError::invalid("Session submission limit reached"));
                }
                let mut next = state.record.clone();
                next.submissions.insert(key, digest);
                app.store.save(&next)?;
                state.record = next;
            }
            if let Some(cancel) = &state.active {
                cancel.cancel();
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn unsupported() -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        "unsupported_endpoint",
        "This endpoint is not supported; see services/api/README.md".into(),
    )
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let token = std::env::var("NANOCODEX_API_TOKEN")?;
    eyre::ensure!(
        token.len() >= 32,
        "NANOCODEX_API_TOKEN must have at least 32 bytes"
    );
    let model_key = std::env::var("OPENAI_API_KEY")?;
    eyre::ensure!(
        !model_key.trim().is_empty(),
        "OPENAI_API_KEY must not be empty"
    );
    let store = Store::open(
        &std::env::var("NANOCODEX_DB").unwrap_or_else(|_| "/data/nanocodex.sqlite".into()),
    )?;
    let mut sessions = BTreeMap::new();
    for mut record in store.load()? {
        for turn in &mut record.turns {
            if turn.status == TurnStatus::InProgress {
                turn.status = TurnStatus::Failed;
                turn.completed_at = Some(now());
                turn.error = Some(TurnError {
                    code: "server_error".into(),
                    message:
                        "The service stopped before this turn completed; submit a new turn to retry"
                            .into(),
                });
            }
        }
        record.session.status = SessionStatus::Idle;
        store
            .save(&record)
            .map_err(|_| eyre::eyre!("Cannot save recovered session"))?;
        sessions.insert(
            record.session.id.clone(),
            Arc::new(Mutex::new(LiveSession::new(record))),
        );
    }
    let app = Arc::new(App {
        store,
        sessions: RwLock::new(sessions),
        slots: Arc::new(Semaphore::new(4)),
        api_token_hash: Sha256::digest(token.as_bytes()).into(),
        model_key,
        model_url: std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
        shutdown: CancellationToken::new(),
    });
    let api = Router::new()
        .route("/agents/sessions", post(create).get(list))
        .route(
            "/agents/sessions/{id}",
            get(retrieve).post(update).delete(delete),
        )
        .route("/agents/sessions/{id}/events", get(subscribe).post(submit))
        .route("/agents/sessions/{id}/items", get(items))
        .route("/agents/sessions/{id}/turns", get(turns))
        .route("/agents/sessions/{id}/turns/{turn_id}", get(turn))
        .fallback(unsupported)
        .method_not_allowed_fallback(unsupported)
        .layer(DefaultBodyLimit::max(131_072))
        .layer(middleware::from_fn_with_state(app.clone(), authenticate));
    let router = Router::new()
        .nest("/v1", api)
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route(
            "/readyz",
            get({
                let app = app.clone();
                move || {
                    let stopping = app.shutdown.is_cancelled();
                    async move {
                        if stopping {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::OK
                        }
                    }
                }
            }),
        )
        .with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(
        std::env::var("NANOCODEX_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
    )
    .await?;
    eprintln!("NanoCodex API ready; environment=none; single-operator scope");
    let stopping = app.shutdown.clone();
    axum::serve(listener, router).with_graceful_shutdown(async move {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = async { if let Some(signal) = &mut terminate { signal.recv().await; } else { std::future::pending::<()>().await; } } => {},
        }
        stopping.cancel();
    }).await?;
    let _ = tokio::time::timeout(
        Duration::from_secs(20),
        app.slots.clone().acquire_many_owned(4),
    )
    .await;
    Ok(())
}
