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
    pub submissions: BTreeMap<String, [u8; 32]>,
}
pub struct Store(Mutex<Connection>);
impl Store {
    pub fn open(path: &str) -> eyre::Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON; CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, record TEXT NOT NULL);")?;
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
        self.0
            .lock()
            .map_err(|_| ApiError::internal())?
            .execute("DELETE FROM sessions WHERE id=?1", [id])
            .map_err(|_| ApiError::internal())?;
        Ok(())
    }
}
