use crate::types::*;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::Path;

pub(crate) struct Store {
    connection: Mutex<Connection>,
}

fn decode<T: DeserializeOwned>(text: String) -> Result<T> {
    Ok(serde_json::from_str(&text)?)
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
        builder
            .create(directory)
            .map_err(|e| AgentError::Storage(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| AgentError::Storage(e.to_string()))?;
        }
        let path = directory.join("sessions.sqlite3");
        // Create the file privately before SQLite opens it. SQLite's WAL and
        // shared-memory files inherit its mode on Unix.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|e| AgentError::Storage(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| AgentError::Storage(e.to_string()))?;
        }
        drop(file);
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_millis(500))?;
        // In exclusive WAL mode SQLite holds the database lock until this
        // connection closes. A second host cannot recover another live host's
        // runs or compete for an operation. CLI clients connect to the host.
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA locking_mode=EXCLUSIVE;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             BEGIN EXCLUSIVE; COMMIT;",
        )?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 1 {
            return Err(AgentError::Storage(
                "this database was written by a newer Switchyard app".into(),
            ));
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY, root TEXT UNIQUE NOT NULL, value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
                value TEXT NOT NULL, transcript TEXT NOT NULL DEFAULT '[]'
             );
             CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                command_id TEXT NOT NULL, fingerprint TEXT NOT NULL, value TEXT NOT NULL,
                UNIQUE(session_id, command_id)
             );
             CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL REFERENCES sessions(id), seq INTEGER NOT NULL,
                value TEXT NOT NULL, PRIMARY KEY(session_id, seq)
             );
             CREATE TABLE IF NOT EXISTS operations (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                run_id TEXT NOT NULL REFERENCES runs(id), state TEXT NOT NULL,
                value TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS sessions_project ON sessions(project_id);
             CREATE INDEX IF NOT EXISTS operations_session ON operations(session_id, state);
             PRAGMA user_version=1;",
        )?;
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.recover()?;
        Ok(store)
    }

    pub(crate) fn transaction<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub(crate) fn projects(&self) -> Result<Vec<Project>> {
        let connection = self.connection.lock();
        let mut query = connection.prepare("SELECT value FROM projects ORDER BY rowid DESC")?;
        let rows = query.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|row| decode(row?)).collect()
    }

    pub(crate) fn sessions(&self, project: Option<&str>) -> Result<Vec<Session>> {
        let connection = self.connection.lock();
        let mut query = connection.prepare(
            "SELECT value FROM sessions WHERE (?1 IS NULL OR project_id=?1) ORDER BY rowid DESC",
        )?;
        let rows = query.query_map([project], |r| r.get::<_, String>(0))?;
        rows.map(|row| decode(row?)).collect()
    }

    pub(crate) fn project(&self, id: &str) -> Result<Project> {
        get_project(&self.connection.lock(), id)
    }

    pub(crate) fn session(&self, id: &str) -> Result<Session> {
        get_session(&self.connection.lock(), id)
    }

    pub(crate) fn transcript(&self, id: &str) -> Result<Vec<Value>> {
        get_transcript(&self.connection.lock(), id)
    }

    pub(crate) fn events(&self, session: &str, after: u64, limit: usize) -> Result<Vec<Event>> {
        let connection = self.connection.lock();
        get_session(&connection, session)?;
        let mut query = connection.prepare(
            "SELECT value FROM events WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let after = i64::try_from(after)
            .map_err(|_| AgentError::Invalid("event cursor is too large".into()))?;
        let rows = query.query_map(params![session, after, limit as i64], |r| {
            r.get::<_, String>(0)
        })?;
        rows.map(|row| decode(row?)).collect()
    }

    pub(crate) fn pending(&self, session: &str) -> Result<Option<Operation>> {
        let connection = self.connection.lock();
        let text = connection
            .query_row(
                "SELECT value FROM operations WHERE session_id=?1 AND state='awaiting_approval' ORDER BY rowid DESC LIMIT 1",
                [session],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        text.map(decode).transpose()
    }

    pub(crate) fn event(
        &self,
        session: &str,
        run: &str,
        kind: &str,
        payload: Value,
    ) -> Result<Event> {
        self.transaction(|tx| append_event(tx, session, Some(run), kind, payload))
    }

    fn recover(&self) -> Result<()> {
        let sessions = self.sessions(None)?;
        self.transaction(|tx| {
            for mut session in sessions {
                if !matches!(session.state, SessionState::Running | SessionState::AwaitingApproval) {
                    continue;
                }
                let mut query = tx.prepare(
                    "SELECT value FROM operations WHERE session_id=?1 AND state IN ('proposed','awaiting_approval','approved','started')",
                )?;
                let operations = query
                    .query_map([&session.id], |r| r.get::<_, String>(0))?
                    .map(|row| decode::<Operation>(row?))
                    .collect::<Result<Vec<_>>>()?;
                let mut unknown = false;
                for mut operation in operations {
                    let uncertain = operation.state == "started" && operation.requires_approval;
                    unknown |= uncertain;
                    operation.state = if uncertain { "outcome_unknown" } else { "expired" }.into();
                    save_operation(tx, &operation)?;
                }
                // An incomplete assistant tool batch must never be replayed.
                // Resolve any unmatched calls explicitly as unknown/interrupted.
                resolve_unmatched_calls(tx, &session.id, "The app stopped before this tool result was recorded. Do not assume success or rerun a side effect without review.")?;
                let run_id = session.active_run_id.take();
                session.state = if unknown { SessionState::RecoveryRequired } else { SessionState::Interrupted };
                save_session(tx, &session)?;
                if let Some(run_id) = &run_id {
                    finish_run(tx, run_id, session.state.clone())?;
                }
                append_event(tx, &session.id, run_id.as_deref(), "recovery.required", json!({
                    "outcome_unknown": unknown,
                    "message": if unknown {
                        "An operation started before the app stopped; inspect its effects and acknowledge recovery before continuing. It has not been replayed."
                    } else {
                        "The previous run stopped with the app. Pending approvals expired. Submit a new turn to continue."
                    }
                }))?;
            }
            Ok(())
        })
    }
}

pub(crate) fn acknowledge_recovery(
    tx: &Transaction<'_>,
    session_id: &str,
    expected_revision: u64,
    note: &str,
) -> Result<Session> {
    let mut session = get_session(tx, session_id)?;
    if session.state != SessionState::RecoveryRequired || session.active_run_id.is_some() {
        return Err(AgentError::Conflict(
            "this session is not waiting for recovery review".into(),
        ));
    }
    if session.revision != expected_revision {
        return Err(AgentError::Conflict(
            "the session changed; read its latest recovery evidence before acknowledging".into(),
        ));
    }
    session.state = SessionState::Interrupted;
    save_session(tx, &session)?;
    append_event(
        tx,
        session_id,
        None,
        "recovery.acknowledged",
        json!({
            "note":note,
            "outcome_unknown":true,
            "message":"The user reviewed the uncertain operation. Its original outcome is still unknown; no work was automatically replayed."
        }),
    )?;
    get_session(tx, session_id)
}

pub(crate) fn get_project(connection: &Connection, id: &str) -> Result<Project> {
    let text = connection
        .query_row("SELECT value FROM projects WHERE id=?1", [id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    decode(text.ok_or_else(|| AgentError::NotFound("project not found".into()))?)
}

pub(crate) fn get_session(connection: &Connection, id: &str) -> Result<Session> {
    let text = connection
        .query_row("SELECT value FROM sessions WHERE id=?1", [id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    decode(text.ok_or_else(|| AgentError::NotFound("session not found".into()))?)
}

pub(crate) fn save_session(tx: &Transaction<'_>, session: &Session) -> Result<()> {
    tx.execute(
        "UPDATE sessions SET value=?2 WHERE id=?1",
        params![session.id, serde_json::to_string(session)?],
    )?;
    Ok(())
}

pub(crate) fn get_transcript(connection: &Connection, id: &str) -> Result<Vec<Value>> {
    let text = connection
        .query_row("SELECT transcript FROM sessions WHERE id=?1", [id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    decode(text.ok_or_else(|| AgentError::NotFound("session not found".into()))?)
}

pub(crate) fn append_transcript(
    tx: &Transaction<'_>,
    session: &str,
    items: &[Value],
) -> Result<()> {
    let mut transcript = get_transcript(tx, session)?;
    transcript.extend_from_slice(items);
    tx.execute(
        "UPDATE sessions SET transcript=?2 WHERE id=?1",
        params![session, serde_json::to_string(&transcript)?],
    )?;
    Ok(())
}

pub(crate) fn append_event(
    tx: &Transaction<'_>,
    session_id: &str,
    run: Option<&str>,
    kind: &str,
    payload: Value,
) -> Result<Event> {
    let mut session = get_session(tx, session_id)?;
    session.last_seq += 1;
    session.revision += 1;
    session.updated_at_ms = now_ms();
    let event = Event {
        schema_version: 1,
        session_id: session_id.into(),
        run_id: run.map(str::to_owned),
        seq: session.last_seq,
        at_ms: session.updated_at_ms,
        kind: kind.into(),
        payload,
    };
    tx.execute(
        "INSERT INTO events(session_id,seq,value) VALUES(?1,?2,?3)",
        params![session_id, event.seq as i64, serde_json::to_string(&event)?],
    )?;
    save_session(tx, &session)?;
    Ok(event)
}

pub(crate) fn run_by_command(
    tx: &Transaction<'_>,
    session: &str,
    command: &str,
    fingerprint: &str,
) -> Result<Option<Run>> {
    let row = tx
        .query_row(
            "SELECT fingerprint,value FROM runs WHERE session_id=?1 AND command_id=?2",
            params![session, command],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    match row {
        Some((prior, _)) if prior != fingerprint => Err(AgentError::Conflict(
            "command_id was already used with different text".into(),
        )),
        Some((_, value)) => Ok(Some(decode(value)?)),
        None => Ok(None),
    }
}

pub(crate) fn save_operation(tx: &Transaction<'_>, operation: &Operation) -> Result<()> {
    tx.execute("INSERT INTO operations(id,session_id,run_id,state,value) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET state=excluded.state,value=excluded.value", params![operation.id,operation.session_id,operation.run_id,operation.state,serde_json::to_string(operation)?])?;
    Ok(())
}

pub(crate) fn finish_run(tx: &Transaction<'_>, id: &str, state: SessionState) -> Result<()> {
    let mut run: Run =
        decode(tx.query_row("SELECT value FROM runs WHERE id=?1", [id], |r| r.get(0))?)?;
    run.state = state;
    run.ended_at_ms = Some(now_ms());
    tx.execute(
        "UPDATE runs SET value=?2 WHERE id=?1",
        params![id, serde_json::to_string(&run)?],
    )?;
    Ok(())
}

pub(crate) fn resolve_unmatched_calls(
    tx: &Transaction<'_>,
    session: &str,
    reason: &str,
) -> Result<()> {
    let transcript = get_transcript(tx, session)?;
    let mut pending = Vec::new();
    for item in transcript {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    pending.push(id.to_owned());
                }
            }
            Some("function_call_output") => {
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    // Pair in transcript order. Some compatible providers reuse
                    // an ID on a later turn; an older result cannot settle it.
                    if let Some(index) = pending.iter().position(|call| call == id) {
                        pending.remove(index);
                    }
                }
            }
            _ => {}
        }
    }
    let missing: Vec<_> = pending.into_iter().map(|id| json!({"type":"function_call_output","call_id":id,"output":serde_json::to_string(&json!({"status":"interrupted","error":reason})).unwrap_or_default()})).collect();
    append_transcript(tx, session, &missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_running(store: &Store, operation_state: &str, mutating: bool) {
        store.transaction(|tx| {
            let project = Project { id: "p".into(), name: "Project".into(), root: "project".into(), created_at_ms: 1 };
            tx.execute("INSERT INTO projects(id,root,value) VALUES('p','project',?1)", [serde_json::to_string(&project)?])?;
            let session = Session { id: "s".into(), project_id: "p".into(), model: "fixture".into(), title: "Session".into(), state: SessionState::Running, active_run_id: Some("r".into()), revision: 0, last_seq: 0, created_at_ms: 1, updated_at_ms: 1 };
            tx.execute("INSERT INTO sessions(id,project_id,value,transcript) VALUES('s','p',?1,?2)", params![serde_json::to_string(&session)?,json!([{"type":"function_call","call_id":"call1","name":"run_command","arguments":"{}"}]).to_string()])?;
            let run = Run { id: "r".into(), session_id: "s".into(), command_id: "c".into(), state: SessionState::Running, started_at_ms: 1, ended_at_ms: None };
            tx.execute("INSERT INTO runs(id,session_id,command_id,fingerprint,value) VALUES('r','s','c','fingerprint',?1)", [serde_json::to_string(&run)?])?;
            let operation = Operation { id: "op".into(), session_id: "s".into(), run_id: "r".into(), call_id: "call1".into(), name: "run_command".into(), arguments_hash: "hash".into(), arguments: json!({"command":"example"}), requires_approval: mutating, state: operation_state.into(), preview: json!({}) };
            save_operation(tx, &operation)?;
            Ok(())
        }).unwrap();
    }

    #[test]
    fn crash_after_side_effect_start_requires_recovery_and_never_replays() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        seed_running(&store, "started", true);
        drop(store);
        let recovered = Store::open(directory.path()).unwrap();
        let session = recovered.session("s").unwrap();
        assert_eq!(session.state, SessionState::RecoveryRequired);
        assert!(session.active_run_id.is_none());
        let events = recovered.events("s", 0, 50).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "recovery.required");
        assert_eq!(events[0].payload["outcome_unknown"], true);
        let transcript = recovered.transcript("s").unwrap();
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[1]["type"], "function_call_output");
        assert!(
            transcript[1]["output"]
                .as_str()
                .unwrap()
                .contains("Do not assume success")
        );
        drop(recovered);
        let second = Store::open(directory.path()).unwrap();
        assert_eq!(
            second.events("s", 0, 50).unwrap().len(),
            1,
            "recovery is itself idempotent"
        );
    }

    #[test]
    fn a_pending_approval_expires_after_restart_without_unknown_effects() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        seed_running(&store, "awaiting_approval", true);
        drop(store);
        let recovered = Store::open(directory.path()).unwrap();
        assert_eq!(
            recovered.session("s").unwrap().state,
            SessionState::Interrupted
        );
        assert!(recovered.pending("s").unwrap().is_none());
        assert_eq!(
            recovered.events("s", 0, 50).unwrap()[0].payload["outcome_unknown"],
            false
        );
    }

    #[test]
    fn another_host_cannot_open_and_recover_the_live_database() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        assert!(Store::open(directory.path()).is_err());
        drop(store);
        assert!(Store::open(directory.path()).is_ok());
    }

    #[test]
    fn acknowledgement_is_revision_checked_and_preserves_unknown_operations() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        seed_running(&store, "started", true);
        drop(store);
        let store = Store::open(directory.path()).unwrap();
        let before = store.session("s").unwrap();
        assert!(
            store
                .transaction(|tx| acknowledge_recovery(
                    tx,
                    "s",
                    before.revision + 1,
                    "Reviewed files"
                ))
                .is_err()
        );
        assert_eq!(store.session("s").unwrap().revision, before.revision);
        let after = store
            .transaction(|tx| {
                acknowledge_recovery(
                    tx,
                    "s",
                    before.revision,
                    "Reviewed files and command output",
                )
            })
            .unwrap();
        assert_eq!(after.state, SessionState::Interrupted);
        assert_eq!(after.revision, before.revision + 1);
        assert!(after.active_run_id.is_none());
        assert!(
            store
                .transaction(|tx| acknowledge_recovery(tx, "s", after.revision, "Again"))
                .is_err()
        );
        let connection = store.connection.lock();
        let operation: Operation = decode(
            connection
                .query_row("SELECT value FROM operations WHERE id='op'", [], |r| {
                    r.get(0)
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(operation.state, "outcome_unknown");
        let run: Run = decode(
            connection
                .query_row("SELECT value FROM runs WHERE id='r'", [], |r| r.get(0))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(run.state, SessionState::RecoveryRequired);
        drop(connection);
        let events = store.events("s", 0, 50).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, "recovery.acknowledged");
        assert_eq!(events[1].payload["outcome_unknown"], true);
        assert!(
            store.transcript("s").unwrap()[1]["output"]
                .as_str()
                .unwrap()
                .contains("Do not assume success")
        );
    }
}
