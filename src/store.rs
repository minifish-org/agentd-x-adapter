use crate::x::{Includes, Tweet};
use anyhow::{ensure, Result};
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

pub struct Store {
    db: Connection,
    _lock: File,
}
pub struct Job {
    pub id: String,
    pub tweet: Tweet,
    pub includes: Includes,
    pub publish: bool,
    pub run_id: Option<String>,
    pub state: String,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lockfile"))?;
        lock.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("another adapter holds the state lock"))?;
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,tweet TEXT NOT NULL,includes TEXT NOT NULL,publish INTEGER NOT NULL,run_id TEXT,state TEXT NOT NULL DEFAULT 'pending',reply_id TEXT,preview TEXT,payload TEXT,next_at INTEGER NOT NULL DEFAULT 0,updated INTEGER NOT NULL);
            CREATE UNIQUE INDEX IF NOT EXISTS run_job ON jobs(run_id) WHERE run_id IS NOT NULL;")?;
        let store = Self { db, _lock: lock };
        // A crash in the POST interval is ambiguous. Never automatically send again.
        store
            .db
            .execute("UPDATE jobs SET state='unknown' WHERE state='sending'", [])?;
        Ok(store)
    }
    pub fn bind(&self, identity: &str) -> Result<()> {
        if let Some(saved) = self.get("identity")? {
            ensure!(
                saved == identity,
                "state belongs to a different bot/owner/agentd/tenant/agent"
            );
        } else {
            self.set("identity", identity)?;
        }
        if self.get("start")?.is_none() {
            self.set(
                "start",
                &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            )?;
        }
        Ok(())
    }
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
            .optional()?)
    }
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
    pub fn enqueue(&self, t: &Tweet, i: &Includes, publish: bool) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO jobs(id,tweet,includes,publish,updated) VALUES(?,?,?,?,?)",
            params![
                t.id,
                serde_json::to_string(t)?,
                serde_json::to_string(i)?,
                publish,
                chrono::Utc::now().timestamp()
            ],
        )?;
        Ok(())
    }
    pub fn job(&self, id: &str) -> Result<Option<Job>> {
        let row = self
            .db
            .query_row(
                "SELECT id,tweet,includes,publish,run_id,state FROM jobs WHERE id=?",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(id, t, i, publish, run_id, state)| {
            Ok(Job {
                id,
                tweet: serde_json::from_str(&t)?,
                includes: serde_json::from_str(&i)?,
                publish,
                run_id,
                state,
            })
        })
        .transpose()
    }
    pub fn ready(&self) -> Result<Vec<String>> {
        let mut s=self.db.prepare("SELECT id FROM jobs WHERE state IN ('pending','preview_wait') AND next_at<=? ORDER BY length(id),id LIMIT 10")?;
        let rows = s.query_map([chrono::Utc::now().timestamp()], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    pub fn submitted(&self, id: &str, run: &str, publish: bool) -> Result<()> {
        self.db.execute(
            "UPDATE jobs SET run_id=?,state=?,updated=? WHERE id=?",
            params![
                run,
                if publish { "submitted" } else { "preview_wait" },
                chrono::Utc::now().timestamp(),
                id
            ],
        )?;
        Ok(())
    }
    pub fn payload(&self, id: &str) -> Result<Option<Value>> {
        let raw: Option<String> =
            self.db
                .query_row("SELECT payload FROM jobs WHERE id=?", [id], |r| r.get(0))?;
        Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
    }
    pub fn save_payload(&self, id: &str, payload: &Value) -> Result<()> {
        self.db.execute(
            "UPDATE jobs SET payload=? WHERE id=?",
            params![serde_json::to_string(payload)?, id],
        )?;
        Ok(())
    }
    pub fn state(&self, id: &str, state: &str) -> Result<()> {
        self.db.execute(
            "UPDATE jobs SET state=?,updated=? WHERE id=?",
            params![state, chrono::Utc::now().timestamp(), id],
        )?;
        Ok(())
    }
    pub fn defer(&self, id: &str, seconds: u64) -> Result<()> {
        self.db.execute(
            "UPDATE jobs SET next_at=? WHERE id=?",
            params![
                chrono::Utc::now().timestamp() + seconds.min(86400) as i64,
                id
            ],
        )?;
        Ok(())
    }
    pub fn sent(&self, id: &str, reply: &str) -> Result<()> {
        self.db.execute(
            "UPDATE jobs SET state='sent',reply_id=?,updated=? WHERE id=?",
            params![reply, chrono::Utc::now().timestamp(), id],
        )?;
        Ok(())
    }
    pub fn preview_done(&self, id: &str, value: &Value) -> Result<()> {
        self.db.execute("UPDATE jobs SET state='preview',preview=?,tweet='{}',includes='{}',updated=? WHERE id=?",params![serde_json::to_string(value)?,chrono::Utc::now().timestamp(),id])?;
        Ok(())
    }
    pub fn report(&self) -> Result<Value> {
        let mut s = self.db.prepare(
            "SELECT id,state,run_id,reply_id,preview FROM jobs ORDER BY updated DESC LIMIT 50",
        )?;
        let rows=s.query_map([],|r|Ok(serde_json::json!({"post_id":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"run_id":r.get::<_,Option<String>>(2)?,"reply_id":r.get::<_,Option<String>>(3)?,"preview":r.get::<_,Option<String>>(4)?})))?;
        Ok(Value::Array(rows.collect::<rusqlite::Result<_>>()?))
    }
    pub fn prune(&self) -> Result<()> {
        // Keep tiny ID tombstones for idempotency, delete retained tweet/reply text after 7 days.
        self.db.execute("UPDATE jobs SET tweet='{}',includes='{}',preview=NULL,payload=NULL WHERE state IN ('preview','sent','failed') AND updated<?",[chrono::Utc::now().timestamp()-7*86400])?;
        Ok(())
    }
}
