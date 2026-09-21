use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
};
use serde::Deserialize;

use crate::{App, Deleted, body, id, now, protocol::*, query, store::AgentRecord};

pub fn agent(config: &AgentInput, id: String) -> ApiResult<Agent> {
    Ok(Agent {
        id,
        model: config.validate()?.as_str().into(),
        instructions: config.instructions.clone(),
        name: config.name.clone(),
        tools: config
            .tools
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(ToolConfig::public)
            .collect(),
        multi_agent: config.multi_agent.clone().unwrap_or(MultiAgent {
            enabled: false,
            max_concurrent_subagents: Some(6),
        }),
        reasoning: config.reasoning.clone().unwrap_or(Reasoning {
            effort: "medium".into(),
            summary: None,
        }),
        service_tier: "default".into(),
        text: TextConfig {
            format: TextFormat {
                r#type: "text".into(),
            },
            verbosity: "medium".into(),
        },
    })
}

pub async fn resolve(
    app: &App,
    agent_id: Option<&str>,
    inline: Option<AgentInput>,
) -> ApiResult<AgentInput> {
    match (agent_id, inline) {
        (Some(_), Some(_)) => Err(ApiError::invalid("Supply agent or agent_id, not both")),
        (Some(id), None) => app
            .agents
            .read()
            .await
            .get(id)
            .map(|a| a.config.clone())
            .ok_or_else(ApiError::missing),
        (None, Some(config)) => Ok(config),
        (None, None) => Err(ApiError::invalid("Supply agent or agent_id")),
    }
}

pub async fn create_agent(
    State(app): State<Arc<App>>,
    request: Result<Json<AgentInput>, JsonRejection>,
) -> ApiResult<Json<SavedAgent>> {
    Ok(Json(persist(&app, body(request)?).await?))
}
pub async fn persist(app: &App, config: AgentInput) -> ApiResult<SavedAgent> {
    config.validate()?;
    app.validate_tools(config.tools.as_deref().unwrap_or_default())?;
    let public = SavedAgent {
        agent: agent(&config, id("agent"))?,
        object: "agent".into(),
        created_at: now(),
        updated_at: now(),
        metadata: metadata(config.metadata.clone())?,
    };
    let record = AgentRecord {
        public: public.clone(),
        config,
    };
    let mut agents = app.agents.write().await;
    if agents.len() >= 1000 {
        return Err(ApiError::invalid("Service limit is 1000 saved agents"));
    }
    app.store.save_agent(&record)?;
    agents.insert(public.agent.id.clone(), record);
    Ok(public)
}
pub async fn get_agent(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<SavedAgent>> {
    Ok(Json(
        app.agents
            .read()
            .await
            .get(&id)
            .ok_or_else(ApiError::missing)?
            .public
            .clone(),
    ))
}
pub async fn list_agents(
    State(app): State<Arc<App>>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<SavedAgent>>> {
    Ok(Json(
        query(params)?.page(
            app.agents
                .read()
                .await
                .values()
                .map(|a| a.public.clone())
                .collect(),
        )?,
    ))
}

#[derive(Default)]
pub enum Field<T> {
    #[default]
    Missing,
    Present(Option<T>),
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::Present(Option::deserialize(deserializer)?))
    }
}
impl<T> Field<T> {
    fn apply(self, target: &mut Option<T>) {
        if let Self::Present(value) = self {
            *target = value;
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentPatch {
    model: Field<String>,
    instructions: Field<String>,
    name: Field<String>,
    metadata: Field<Metadata>,
    tools: Field<Vec<ToolConfig>>,
    multi_agent: Field<MultiAgent>,
    reasoning: Field<Reasoning>,
}
pub async fn update_agent(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    request: Result<Json<AgentPatch>, JsonRejection>,
) -> ApiResult<Json<SavedAgent>> {
    let patch = body(request)?;
    let mut agents = app.agents.write().await;
    let mut next = agents.get(&id).cloned().ok_or_else(ApiError::missing)?;
    if matches!(patch.model, Field::Present(None)) {
        return Err(ApiError::invalid("model cannot be null"));
    }
    patch.model.apply(&mut next.config.model);
    patch.instructions.apply(&mut next.config.instructions);
    patch.name.apply(&mut next.config.name);
    patch.metadata.apply(&mut next.config.metadata);
    patch.tools.apply(&mut next.config.tools);
    patch.multi_agent.apply(&mut next.config.multi_agent);
    patch.reasoning.apply(&mut next.config.reasoning);
    next.config.validate()?;
    app.validate_tools(next.config.tools.as_deref().unwrap_or_default())?;
    next.public.agent = agent(&next.config, id.clone())?;
    next.public.metadata = metadata(next.config.metadata.clone())?;
    next.public.updated_at = now();
    app.store.save_agent(&next)?;
    let public = next.public.clone();
    agents.insert(id, next);
    Ok(Json(public))
}
pub async fn delete_agent(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Deleted>> {
    let mut agents = app.agents.write().await;
    if !agents.contains_key(&id) {
        return Err(ApiError::missing());
    }
    app.store.delete_agent(&id)?;
    agents.remove(&id);
    Ok(Json(Deleted {
        id,
        object: "agent.deleted",
        deleted: true,
    }))
}

fn child(record: &crate::store::Record, id: &str) -> ApiResult<Subagent> {
    record
        .subagents
        .iter()
        .find(|s| s.id == id)
        .cloned()
        .ok_or_else(ApiError::missing)
}
pub async fn subagents(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Subagent>>> {
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    Ok(Json(query(params)?.page(state.record.subagents.clone())?))
}
pub async fn subagent(
    State(app): State<Arc<App>>,
    Path((id, child_id)): Path<(String, String)>,
) -> ApiResult<Json<Subagent>> {
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    Ok(Json(child(&state.record, &child_id)?))
}
pub async fn child_items(
    State(app): State<Arc<App>>,
    Path((id, child_id)): Path<(String, String)>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Item>>> {
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    child(&state.record, &child_id)?;
    let items = state
        .record
        .items
        .iter()
        .filter(|item| {
            state
                .record
                .turns
                .iter()
                .any(|t| t.id == item.turn_id() && t.subagent_id.as_deref() == Some(&child_id))
        })
        .cloned()
        .collect();
    Ok(Json(query(params)?.page(items)?))
}
pub async fn child_turns(
    State(app): State<Arc<App>>,
    Path((id, child_id)): Path<(String, String)>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Turn>>> {
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    child(&state.record, &child_id)?;
    Ok(Json(
        query(params)?.page(
            state
                .record
                .turns
                .iter()
                .filter(|t| t.subagent_id.as_deref() == Some(&child_id))
                .cloned()
                .collect(),
        )?,
    ))
}
pub async fn child_turn(
    State(app): State<Arc<App>>,
    Path((id, child_id, turn_id)): Path<(String, String, String)>,
) -> ApiResult<Json<Turn>> {
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    Ok(Json(
        state
            .record
            .turns
            .iter()
            .find(|t| t.id == turn_id && t.subagent_id.as_deref() == Some(&child_id))
            .cloned()
            .ok_or_else(ApiError::missing)?,
    ))
}
pub async fn child_turn_items(
    State(app): State<Arc<App>>,
    Path((id, child_id, turn_id)): Path<(String, String, String)>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Item>>> {
    let live = app.session(&id).await?;
    let state = live.lock().await;
    state.session()?;
    if !state
        .record
        .turns
        .iter()
        .any(|t| t.id == turn_id && t.subagent_id.as_deref() == Some(&child_id))
    {
        return Err(ApiError::missing());
    }
    Ok(Json(
        query(params)?.page(
            state
                .record
                .items
                .iter()
                .filter(|i| i.turn_id() == turn_id)
                .cloned()
                .collect(),
        )?,
    ))
}
