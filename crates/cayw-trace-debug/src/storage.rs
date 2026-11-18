//! Transactional trace capture: append raw events, replace only changed projects.
use crate::{Document, Error, Metrics, Project, SessionCapture};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

#[derive(Serialize, Deserialize)]
struct Header {
    schema: String,
    sequence: u64,
    sessions: BTreeMap<String, SessionCapture>,
    metrics: Metrics,
}

pub(crate) fn open(path: &Path) -> Result<(Connection, Document, bool), Error> {
    let sqlite = match File::open(path) {
        Ok(mut file) => {
            let mut magic = [0; 16];
            file.read(&mut magic)? == 16 && &magic == b"SQLite format 3\0"
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if sqlite {
        let connection = Connection::open(path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 1 {
            return Err(Error::Invalid(
                "Trace storage version is unsupported".into(),
            ));
        }
        let header: String =
            connection.query_row("SELECT body FROM trace_header WHERE id=1", [], |r| r.get(0))?;
        let h: Header = serde_json::from_str(&header)?;
        let mut document = Document {
            schema: h.schema,
            sequence: h.sequence,
            sessions: h.sessions,
            metrics: h.metrics,
            projects: BTreeMap::new(),
            events: vec![],
        };
        {
            let mut q = connection.prepare("SELECT id,body FROM trace_projects ORDER BY id")?;
            for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (id, body) = row?;
                let p: Project = serde_json::from_str(&body)?;
                if id != p.id {
                    return Err(Error::Invalid("Trace project identity mismatch".into()));
                }
                document.projects.insert(id, p);
            }
        }
        return Ok((connection, document, false));
    }
    let (mut document, migrated) = crate::read_document(path)?;
    if document.schema != crate::TRACE_SCHEMA {
        return Err(Error::Invalid("Trace schema is unsupported".into()));
    }
    if path.exists() {
        crate::archive_document(path, "json-evidence.json")?;
    }
    let temporary = path.with_extension(format!("{}.sqlite.tmp", uuid::Uuid::new_v4()));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        let mut connection = Connection::open(&temporary)?;
        connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA user_version=1;
            CREATE TABLE trace_header(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);
            CREATE TABLE trace_projects(id TEXT PRIMARY KEY,body TEXT NOT NULL);
            CREATE TABLE trace_events(ordinal INTEGER PRIMARY KEY,session TEXT NOT NULL,cursor TEXT NOT NULL,body TEXT NOT NULL,UNIQUE(session,cursor));")?;
        persist(&mut connection, &document, &document.events)?;
        drop(connection);
        File::open(&temporary)?.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
        Ok::<_, Error>(())
    })();
    let cleanup = if temporary.exists() {
        std::fs::remove_file(&temporary)
    } else {
        Ok(())
    };
    result?;
    cleanup?;
    document.events.clear();
    Ok((Connection::open(path)?, document, migrated))
}

pub(crate) fn event_count(connection: &Connection) -> Result<u64, Error> {
    let ordinal: i64 = connection.query_row(
        "SELECT COALESCE(MAX(ordinal),0) FROM trace_events",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(ordinal).map_err(|_| Error::Invalid("Trace event ordinal is invalid".into()))
}

pub(crate) fn event_window(
    connection: &Connection,
    after: u64,
    end: u64,
) -> Result<Vec<cayw_core::EventRecord>, Error> {
    let mut query = connection.prepare(
        "SELECT session,cursor,body FROM trace_events WHERE ordinal>?1 AND ordinal<=?2 ORDER BY ordinal"
    )?;
    let mut rows = query.query(params![after as i64, end as i64])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        let session: String = row.get(0)?;
        let cursor: String = row.get(1)?;
        let body: String = row.get(2)?;
        events.push(decode_event(&session, &cursor, &body)?);
    }
    Ok(events)
}

pub(crate) fn events(connection: &Connection) -> Result<Vec<cayw_core::EventRecord>, Error> {
    event_window(connection, 0, event_count(connection)?)
}

fn decode_event(session: &str, cursor: &str, body: &str) -> Result<cayw_core::EventRecord, Error> {
    let event: cayw_core::EventRecord = serde_json::from_str(body)?;
    if event.session_id != session
        || event.cursor.to_string() != cursor
        || event.protocol_version != cayw_core::PROTOCOL_VERSION
    {
        return Err(Error::Invalid(
            "Trace event identity or protocol mismatch".into(),
        ));
    }
    Ok(event)
}

pub(crate) fn event(
    connection: &Connection,
    session: &str,
    cursor: u64,
) -> Result<Option<cayw_core::EventRecord>, Error> {
    let body: Option<String> = connection
        .query_row(
            "SELECT body FROM trace_events WHERE session=?1 AND cursor=?2",
            params![session, cursor.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    body.map(|body| decode_event(session, &cursor.to_string(), &body))
        .transpose()
}

pub(crate) fn persist(
    connection: &mut Connection,
    document: &Document,
    events: &[cayw_core::EventRecord],
) -> Result<(), Error> {
    let tx = connection.transaction()?;
    let header = Header {
        schema: document.schema.clone(),
        sequence: document.sequence,
        sessions: document.sessions.clone(),
        metrics: document.metrics.clone(),
    };
    tx.execute("INSERT INTO trace_header(id,body) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body",[serde_json::to_string(&header)?])?;
    {
        let mut project=tx.prepare("INSERT INTO trace_projects(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body WHERE body != excluded.body")?;
        for (id, value) in &document.projects {
            project.execute(params![id, serde_json::to_string(value)?])?;
        }
        let mut append =
            tx.prepare("INSERT INTO trace_events(session,cursor,body) VALUES(?1,?2,?3)")?;
        for event in events {
            append.execute(params![
                event.session_id,
                event.cursor.to_string(),
                serde_json::to_string(event)?
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}
