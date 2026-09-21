use crate::protocol::*;
use nanocodex::agent::session::SessionSnapshot;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Clone, Deserialize, Serialize)]
pub struct Record {
    pub session: Session,
    pub turns: Vec<Turn>,
    pub items: Vec<Item>,
    pub snapshot: Option<SessionSnapshot>,
    #[serde(default)]
    pub tool_config: Vec<ToolConfig>,
    #[serde(default)]
    pub subagents: Vec<Subagent>,
    pub submissions: BTreeMap<String, [u8; 32]>,
}
pub struct Store(Mutex<Connection>);
impl Store {
    pub fn open(path: &str) -> eyre::Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON; CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, record TEXT NOT NULL); CREATE TABLE IF NOT EXISTS agents (id TEXT PRIMARY KEY, record TEXT NOT NULL); CREATE TABLE IF NOT EXISTS memory (agent_id TEXT PRIMARY KEY, revision INTEGER NOT NULL, files TEXT NOT NULL); CREATE TABLE IF NOT EXISTS memory_versions (agent_id TEXT NOT NULL, revision INTEGER NOT NULL, files TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY(agent_id,revision)); CREATE TABLE IF NOT EXISTS memory_material (position INTEGER PRIMARY KEY AUTOINCREMENT, agent_id TEXT NOT NULL, turn_id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL, prompt TEXT NOT NULL, result TEXT NOT NULL, finished_at INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS memory_material_agent ON memory_material(agent_id,position);")?;
        Ok(Self(Mutex::new(db)))
    }
    pub fn load(&self) -> eyre::Result<Vec<Record>> {
        let db = self
            .0
            .lock()
            .map_err(|_| eyre::eyre!("Database lock failed"))?;
        let mut query = db.prepare("SELECT record FROM sessions ORDER BY id")?;
        let rows = query.query_map([], |r| r.get::<_, String>(0))?;
        let mut records = Vec::new();
        for row in rows {
            records.push(serde_json::from_str(&row?)?);
        }
        Ok(records)
    }
    pub fn save(&self, record: &Record) -> ApiResult<()> {
        let json = serde_json::to_string(record).map_err(|_| ApiError::internal())?;
        let db = self.0.lock().map_err(|_| ApiError::internal())?;
        db.execute("INSERT INTO sessions (id,record) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![record.session.id, json]).map_err(|_| ApiError::internal())?;
        Ok(())
    }
    pub fn delete(&self, id: &str) -> ApiResult<()> {
        let mut db = self.0.lock().map_err(|_| ApiError::internal())?;
        let tx = db.transaction().map_err(|_| ApiError::internal())?;
        let owner: String = tx
            .query_row(
                "SELECT json_extract(record, '$.session.agent.id') FROM sessions WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .map_err(|_| ApiError::internal())?;
        tx.execute("DELETE FROM sessions WHERE id=?1", [id])
            .map_err(|_| ApiError::internal())?;
        prune_memory(&tx, &owner)?;
        tx.commit().map_err(|_| ApiError::internal())
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct AgentRecord {
    pub public: SavedAgent,
    pub config: AgentInput,
}
impl Store {
    pub fn load_agents(&self) -> eyre::Result<BTreeMap<String, AgentRecord>> {
        let db = self
            .0
            .lock()
            .map_err(|_| eyre::eyre!("Database lock failed"))?;
        let mut query = db.prepare("SELECT id,record FROM agents ORDER BY id")?;
        let rows = query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut result = BTreeMap::new();
        for row in rows {
            let (id, json) = row?;
            result.insert(id, serde_json::from_str(&json)?);
        }
        Ok(result)
    }
    pub fn save_agent(&self, agent: &AgentRecord) -> ApiResult<()> {
        let json = serde_json::to_string(agent).map_err(|_| ApiError::internal())?;
        self.0.lock().map_err(|_| ApiError::internal())?.execute("INSERT INTO agents(id,record) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![agent.public.agent.id, json]).map_err(|_| ApiError::internal())?;
        Ok(())
    }
    pub fn delete_agent(&self, id: &str) -> ApiResult<()> {
        let mut db = self.0.lock().map_err(|_| ApiError::internal())?;
        let tx = db.transaction().map_err(|_| ApiError::internal())?;
        tx.execute("DELETE FROM agents WHERE id=?1", [id])
            .map_err(|_| ApiError::internal())?;
        prune_memory(&tx, id)?;
        tx.commit().map_err(|_| ApiError::internal())
    }
}

#[derive(Clone, Serialize)]
pub struct MemoryView {
    pub agent_id: String,
    pub revision: i64,
    pub files: BTreeMap<String, String>,
    pub pending_tasks: i64,
}

fn read_memory(db: &Connection, owner: &str) -> ApiResult<(i64, nanocodex_memory::Bundle)> {
    use rusqlite::OptionalExtension;
    let stored: Option<(i64, String)> = db
        .query_row(
            "SELECT revision, files FROM memory WHERE agent_id=?1",
            [owner],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| ApiError::internal())?;
    match stored {
        None => Ok((0, nanocodex_memory::Bundle::new())),
        Some((revision, files)) => Ok((
            revision,
            nanocodex_memory::Bundle::from_files(
                serde_json::from_str::<BTreeMap<String, String>>(&files)
                    .map_err(|_| ApiError::internal())?,
            ),
        )),
    }
}
fn write_memory(
    db: &Connection,
    owner: &str,
    revision: i64,
    bundle: &nanocodex_memory::Bundle,
) -> ApiResult<()> {
    let files: BTreeMap<_, _> = bundle.files().collect();
    let files = serde_json::to_string(&files).map_err(|_| ApiError::internal())?;
    db.execute("INSERT INTO memory(agent_id,revision,files) VALUES (?1,?2,?3) ON CONFLICT(agent_id) DO UPDATE SET revision=excluded.revision,files=excluded.files", params![owner,revision,files]).map_err(|_| ApiError::internal())?;
    db.execute(
        "INSERT INTO memory_versions(agent_id,revision,files,created_at) VALUES (?1,?2,?3,?4)",
        params![owner, revision, files, crate::now()],
    )
    .map_err(|_| ApiError::internal())?;
    Ok(())
}
impl Store {
    pub fn memory(&self, owner: &str) -> ApiResult<MemoryView> {
        let db = self.0.lock().map_err(|_| ApiError::internal())?;
        let (revision, bundle) = read_memory(&db, owner)?;
        let after = bundle.meta().last_task_position.unwrap_or(0);
        let pending_tasks = db
            .query_row(
                "SELECT count(*) FROM memory_material WHERE agent_id=?1 AND position>?2",
                params![owner, after],
                |r| r.get(0),
            )
            .map_err(|_| ApiError::internal())?;
        Ok(MemoryView {
            agent_id: owner.into(),
            revision,
            files: bundle
                .files()
                .map(|(p, c)| (p.to_owned(), c.to_owned()))
                .collect(),
            pending_tasks,
        })
    }
    pub fn material(
        &self,
        owner: &str,
        after: i64,
    ) -> ApiResult<(Vec<nanocodex_memory::TaskMaterial>, Option<i64>)> {
        let db = self.0.lock().map_err(|_| ApiError::internal())?;
        let mut query = db.prepare("SELECT position,session_id,prompt,result,finished_at FROM memory_material WHERE agent_id=?1 AND position>?2 ORDER BY position LIMIT 20").map_err(|_| ApiError::internal())?;
        let rows = query
            .query_map(params![owner, after], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(|_| ApiError::internal())?;
        let mut tasks = Vec::new();
        let mut position = None;
        let mut bytes = 0;
        for row in rows {
            let (index, thread, prompt, result, time) = row.map_err(|_| ApiError::internal())?;
            bytes += prompt.len() + result.len();
            if bytes > 524_288 && !tasks.is_empty() {
                break;
            }
            position = Some(index);
            tasks.push(nanocodex_memory::TaskMaterial {
                thread,
                prompt,
                result,
                finished_at: chrono::DateTime::from_timestamp(time, 0),
            });
        }
        Ok((tasks, position))
    }
    pub fn finish(&self, record: &Record, turn_id: &str, journal: &str) -> ApiResult<()> {
        let json = serde_json::to_string(record).map_err(|_| ApiError::internal())?;
        let mut db = self.0.lock().map_err(|_| ApiError::internal())?;
        let tx = db.transaction().map_err(|_| ApiError::internal())?;
        tx.execute(
            "UPDATE sessions SET record=?2 WHERE id=?1",
            params![record.session.id, json],
        )
        .map_err(|_| ApiError::internal())?;
        if let Some(turn) = record
            .turns
            .iter()
            .find(|t| t.id == turn_id && t.status == TurnStatus::Completed)
        {
            let owner = &record.session.agent.id;
            let messages: Vec<_> = record
                .items
                .iter()
                .filter_map(|i| {
                    if let Item::Message(m) = i {
                        (m.turn_id == turn_id).then_some(m)
                    } else {
                        None
                    }
                })
                .collect();
            let text = |role: &str| {
                messages
                    .iter()
                    .filter(|m| m.role == role)
                    .flat_map(|m| m.content.iter().map(|c| c.text.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            tx.execute("INSERT OR IGNORE INTO memory_material(agent_id,turn_id,session_id,prompt,result,finished_at) VALUES (?1,?2,?3,?4,?5,?6)",params![owner,turn_id,record.session.id,text("user"),format!("{}\n\nAgent journal notes:\n{}", text("assistant"), journal),turn.completed_at.unwrap_or_else(crate::now)]).map_err(|_| ApiError::internal())?;
            if !journal.is_empty() {
                let (revision, mut bundle) = read_memory(&tx, owner)?;
                let path = nanocodex_memory::Bundle::journal_path(chrono::Utc::now().date_naive());
                let mut content = bundle.get(&path).unwrap_or_default().to_owned();
                content.push_str(journal);
                if content.chars().count() <= nanocodex_memory::bundle::MAX_NOTE_CHARS {
                    bundle.insert(path, content);
                    if bundle.size_bytes() <= nanocodex_memory::bundle::MAX_BUNDLE_BYTES {
                        write_memory(&tx, owner, revision + 1, &bundle)?;
                    }
                }
            }
        }
        tx.commit().map_err(|_| ApiError::internal())
    }
    pub fn commit_memory(
        &self,
        owner: &str,
        expected: i64,
        bundle: &nanocodex_memory::Bundle,
    ) -> ApiResult<()> {
        let mut db = self.0.lock().map_err(|_| ApiError::internal())?;
        let tx = db.transaction().map_err(|_| ApiError::internal())?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE id=?1)",
                [owner],
                |r| r.get(0),
            )
            .map_err(|_| ApiError::internal())?;
        if !exists {
            return Err(ApiError::missing());
        }
        let (revision, _) = read_memory(&tx, owner)?;
        if revision != expected {
            return Err(ApiError::conflict(
                "Memory changed during reconciliation; retry",
            ));
        }
        write_memory(&tx, owner, revision + 1, bundle)?;
        tx.commit().map_err(|_| ApiError::internal())
    }
    pub fn clear_memory(&self, owner: &str) -> ApiResult<()> {
        let mut db = self.0.lock().map_err(|_| ApiError::internal())?;
        let tx = db.transaction().map_err(|_| ApiError::internal())?;
        let (revision, _) = read_memory(&tx, owner)?;
        tx.execute("DELETE FROM memory_material WHERE agent_id=?1", [owner])
            .map_err(|_| ApiError::internal())?;
        tx.execute("DELETE FROM memory_versions WHERE agent_id=?1", [owner])
            .map_err(|_| ApiError::internal())?;
        write_memory(&tx, owner, revision + 1, &nanocodex_memory::Bundle::new())?;
        tx.commit().map_err(|_| ApiError::internal())
    }
}

#[nanocodex::tools::contract::async_trait]
impl nanocodex_memory::MemoryStore for Store {
    async fn load(&self, owner: &str) -> anyhow::Result<nanocodex_memory::Bundle> {
        let view = self
            .memory(owner)
            .map_err(|_| anyhow::anyhow!("Cannot load agent memory"))?;
        Ok(nanocodex_memory::Bundle::from_files(view.files))
    }
    async fn save(&self, owner: &str, changes: &nanocodex_memory::Changes) -> anyhow::Result<()> {
        let view = self
            .memory(owner)
            .map_err(|_| anyhow::anyhow!("Cannot load agent memory"))?;
        let mut bundle = nanocodex_memory::Bundle::from_files(view.files);
        changes.apply(&mut bundle);
        self.commit_memory(owner, view.revision, &bundle)
            .map_err(|_| anyhow::anyhow!("Cannot save agent memory"))
    }
}

// Session snapshots keep their memory until both the saved agent and its sessions are gone.
fn prune_memory(db: &Connection, owner: &str) -> ApiResult<()> {
    let retained: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM agents WHERE id=?1) OR EXISTS(SELECT 1 FROM sessions WHERE json_extract(record, '$.session.agent.id')=?1)", [owner], |r| r.get(0)).map_err(|_| ApiError::internal())?;
    if !retained {
        for table in ["memory_material", "memory_versions", "memory"] {
            db.execute(&format!("DELETE FROM {table} WHERE agent_id=?1"), [owner])
                .map_err(|_| ApiError::internal())?;
        }
    }
    Ok(())
}
