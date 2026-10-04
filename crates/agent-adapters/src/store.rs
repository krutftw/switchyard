//! Durable local record of Codex adapter runs, shared by every client of one host.
//!
//! The Codex CLI keeps the model-side thread in its own profile directory. This
//! store keeps Switchya's side: the run view, its bounded event window, the
//! conversation chain and the trusted account binding needed to resume safely.
use crate::{AdapterError, AdapterEvent, AdapterRun, ProfileBinding, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "adapter-runs.sqlite3";
const SCHEMA_VERSION: u32 = 1;

pub(crate) struct Store {
    connection: Mutex<Connection>,
}

pub(crate) struct LoadedRun {
    pub(crate) view: AdapterRun,
    pub(crate) request_hash: String,
    pub(crate) profile: Option<ProfileBinding>,
}

/// Trusted, host-local binding. It is written only to this private database and
/// never serialized into an HTTP response.
#[derive(Serialize, Deserialize)]
struct StoredProfile {
    id: String,
    name: String,
    agent_id: String,
    home: PathBuf,
    managed: bool,
}

fn storage(error: impl std::fmt::Display) -> AdapterError {
    AdapterError::Unavailable(format!("Agent run history storage failed: {error}"))
}

impl Store {
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory).map_err(storage)?;
        let path = directory.join(FILE_NAME);
        // Create the file privately before SQLite opens it; WAL and shared-memory
        // files inherit its mode on Unix.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(storage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(storage)?;
        }
        drop(file);
        let connection = Connection::open(path).map_err(storage)?;
        connection
            .busy_timeout(std::time::Duration::from_millis(500))
            .map_err(storage)?;
        // Exclusive WAL mode keeps a second host from recovering or appending to
        // runs that a live host still owns. NORMAL synchronization is durable
        // across application crashes; streamed output is written frequently.
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA locking_mode=EXCLUSIVE;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA foreign_keys=ON;
                 BEGIN EXCLUSIVE; COMMIT;",
            )
            .map_err(|_| {
                AdapterError::Unavailable(
                    "Agent run history is in use by another Switchya host. Close it first.".into(),
                )
            })?;
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(storage)?;
        if version > SCHEMA_VERSION {
            return Err(AdapterError::Unavailable(
                "Agent run history was written by a newer Switchya version.".into(),
            ));
        }
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS runs (
                    id TEXT PRIMARY KEY,
                    command_id TEXT UNIQUE NOT NULL,
                    conversation_id TEXT NOT NULL,
                    request_hash TEXT NOT NULL,
                    profile TEXT,
                    value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS events (
                    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                    seq INTEGER NOT NULL,
                    value TEXT NOT NULL,
                    PRIMARY KEY(run_id, seq)
                 );
                 CREATE INDEX IF NOT EXISTS runs_conversation ON runs(conversation_id);
                 CREATE TABLE IF NOT EXISTS retired_commands (
                    command_id TEXT PRIMARY KEY
                 ) WITHOUT ROWID;
                 PRAGMA user_version=1;",
            )
            .map_err(storage)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction().map_err(storage)?;
        let value = f(&tx)?;
        tx.commit().map_err(storage)?;
        Ok(value)
    }

    pub(crate) fn insert_run(
        &self,
        view: &AdapterRun,
        request_hash: &str,
        profile: Option<&ProfileBinding>,
        events: &[AdapterEvent],
    ) -> Result<()> {
        let profile = profile
            .map(|p| {
                serde_json::to_string(&StoredProfile {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    agent_id: p.agent_id.clone(),
                    home: p.home.clone(),
                    managed: p.managed,
                })
            })
            .transpose()
            .map_err(storage)?;
        self.transaction(|tx| {
            tx.execute(
                "INSERT INTO runs(id,command_id,conversation_id,request_hash,profile,value) VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    view.id,
                    view.command_id,
                    view.conversation_id,
                    request_hash,
                    profile,
                    serde_json::to_string(view).map_err(storage)?
                ],
            )
            .map_err(storage)?;
            for event in events {
                insert_event(tx, event)?;
            }
            Ok(())
        })
    }

    /// One transaction per recorded event: the event, the run view it advanced,
    /// and any rows that left the bounded window or were coalesced.
    pub(crate) fn record(
        &self,
        view: &AdapterRun,
        event: Option<&AdapterEvent>,
        removed: &[u64],
    ) -> Result<()> {
        self.transaction(|tx| {
            if let Some(event) = event {
                insert_event(tx, event)?;
            }
            for seq in removed {
                tx.execute(
                    "DELETE FROM events WHERE run_id=?1 AND seq=?2",
                    params![view.id, i64::try_from(*seq).map_err(storage)?],
                )
                .map_err(storage)?;
            }
            let changed = tx
                .execute(
                    "UPDATE runs SET value=?2 WHERE id=?1",
                    params![view.id, serde_json::to_string(view).map_err(storage)?],
                )
                .map_err(storage)?;
            if changed != 1 {
                return Err(storage("the run record is missing"));
            }
            Ok(())
        })
    }

    pub(crate) fn events(
        &self,
        run_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<AdapterEvent>> {
        let connection = self.connection.lock();
        let mut query = connection
            .prepare("SELECT value FROM events WHERE run_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3")
            .map_err(storage)?;
        let after = i64::try_from(after).map_err(storage)?;
        let rows = query
            .query_map(params![run_id, after, limit as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(storage)?;
        rows.map(|row| serde_json::from_str(&row.map_err(storage)?).map_err(storage))
            .collect()
    }

    pub(crate) fn load(&self) -> Result<Vec<LoadedRun>> {
        let connection = self.connection.lock();
        let mut query = connection
            .prepare("SELECT value, request_hash, profile FROM runs ORDER BY rowid")
            .map_err(storage)?;
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(storage)?;
        let mut loaded = Vec::new();
        for row in rows {
            let (value, request_hash, profile) = row.map_err(storage)?;
            let view: AdapterRun = serde_json::from_str(&value).map_err(storage)?;
            let profile = profile
                .map(|text| serde_json::from_str::<StoredProfile>(&text))
                .transpose()
                .map_err(storage)?
                .map(|p| ProfileBinding {
                    id: p.id,
                    name: p.name,
                    agent_id: p.agent_id,
                    home: p.home,
                    managed: p.managed,
                });
            loaded.push(LoadedRun {
                view,
                request_hash,
                profile,
            });
        }
        Ok(loaded)
    }

    /// Removes a whole conversation. Its command IDs are retired in the same
    /// transaction so a delayed client retry can never start that work again.
    pub(crate) fn delete_conversation(&self, conversation_id: &str) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO retired_commands(command_id) SELECT command_id FROM runs WHERE conversation_id=?1",
                [conversation_id],
            )
            .map_err(storage)?;
            tx.execute(
                "DELETE FROM events WHERE run_id IN (SELECT id FROM runs WHERE conversation_id=?1)",
                [conversation_id],
            )
            .map_err(storage)?;
            tx.execute(
                "DELETE FROM runs WHERE conversation_id=?1",
                [conversation_id],
            )
            .map_err(storage)?;
            Ok(())
        })
    }

    pub(crate) fn retired(&self) -> Result<Vec<String>> {
        let connection = self.connection.lock();
        let mut query = connection
            .prepare("SELECT command_id FROM retired_commands")
            .map_err(storage)?;
        let rows = query
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(storage)?;
        rows.map(|row| row.map_err(storage)).collect()
    }
}

fn insert_event(tx: &Transaction<'_>, event: &AdapterEvent) -> Result<()> {
    tx.execute(
        "INSERT INTO events(run_id,seq,value) VALUES(?1,?2,?3)",
        params![
            event.run_id,
            i64::try_from(event.seq).map_err(storage)?,
            serde_json::to_string(event).map_err(storage)?
        ],
    )
    .map_err(storage)?;
    Ok(())
}
