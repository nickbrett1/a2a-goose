//! Answering the hub's `history.*` queries from goose's **own** sessions
//! database.
//!
//! The hub is a router, not a store ([`crate::tunnel`]), and this agent holds no
//! conversation state ([`crate::lib`]). The record is goose's `sessions.db` —
//! the same file `goose serve` writes — so this module is a **read-only** window
//! onto it. It never writes: the database is goose's live file, it may be held
//! open by a running session, and this process has no business migrating,
//! vacuuming or creating anything in it.
//!
//! The four methods and their shapes are the hub's (`roost/src/fake_agent.rs`
//! defines them; the UI already renders them), not ours:
//!
//! | method | body |
//! |---|---|
//! | `history.sessions {cwd?, q?, limit?}` | `{sessions: [<session>], nextCursor: null}` |
//! | `history.session {id}` | `{session: <session>}`, or `{session: null}` |
//! | `history.messages {id?, cursor?, limit?}` | `{sessionId, messages: [<message>], nextCursor}` |
//! | `history.search {q?, limit?}` | `{matches: [{sessionId, name, matches: [<message>]}]}` |
//!
//! A `<session>` is `{sessionId, name, workingDir, createdAt, updatedAt,
//! messageCount, tokens, cost}`; a `<message>` is `{index, role, createdAt,
//! text}`. We map goose's columns onto those fields verbatim where we can and
//! say so in the comments where we have to make a choice.
//!
//! **Fail open.** A missing file, a locked file or an unexpected schema is an
//! error for *that query only* ([`HistoryStore::answer`] returns `Err`, the
//! tunnel turns it into a `refused` response) — never a crash, never a retry
//! loop, and never a write. The host stays up.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::OptionalExtension as _;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

/// The environment variable that overrides goose's sessions database. Set it in
/// a test (or on an unusual host) to point at a database elsewhere.
pub const SESSIONS_DB_ENV: &str = "GOOSE_SESSIONS_DB";

/// `history.sessions` default page size, from the hub's fake agent.
const DEFAULT_SESSIONS_LIMIT: u64 = 50;
/// `history.messages` default page size, from the hub's fake agent (its default
/// is deliberately small: a transcript is paged).
const DEFAULT_MESSAGES_LIMIT: u64 = 2;
/// `history.search` default page size, from the hub's fake agent.
const DEFAULT_SEARCH_LIMIT: u64 = 50;
/// The hard ceiling on any caller-supplied `limit`, so a hub cannot ask this
/// process to materialise an unbounded result.
const MAX_LIMIT: u64 = 500;
/// How many raw `content_json` rows `history.search` will scan per returned
/// session. A prefilter, not a page: a row only becomes a match if its
/// *extracted* text contains the query.
const SEARCH_SCAN_PER_SESSION: u64 = 50;

/// Where goose keeps its sessions database on this host.
///
/// goose resolves its data directory through the XDG base-directory spec
/// (`etcetera`), so this mirrors that rather than guessing a fixed path: the
/// `GOOSE_SESSIONS_DB` override wins, then `$XDG_DATA_HOME/goose/...`, then the
/// default `~/.local/share/goose/...`. A host that has moved its data directory
/// is one `GOOSE_SESSIONS_DB` away from being right.
pub fn default_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os(SESSIONS_DB_ENV) {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from(".local/share"));
    base.join("goose/sessions/sessions.db")
}

/// A read-only handle on goose's sessions database.
///
/// The path is resolved once, at construction, and every query opens its own
/// short-lived connection. That is deliberate: this process must not hold a lock
/// on goose's live file between queries, and a connection-per-query is the
/// cheapest way to guarantee it.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    db_path: PathBuf,
}

impl HistoryStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Answer one `history.*` method, or an error for that query alone.
    ///
    /// `method` must be one of `history.sessions`, `history.session`,
    /// `history.messages`, `history.search`; anything else is an error rather
    /// than an empty body, so a caller never mistakes "unknown" for "nothing".
    pub fn answer(&self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "history.sessions" => self.sessions(params),
            "history.session" => self.session(params),
            "history.messages" => self.messages(params),
            "history.search" => self.search(params),
            other => anyhow::bail!("unknown history method: {other}"),
        }
    }

    /// Open goose's database **read-only**.
    ///
    /// `SQLITE_OPEN_READ_ONLY` is the hard guarantee: even a bug here cannot
    /// write. `PRAGMA query_only` is belt-and-braces on top of it, and the
    /// short busy timeout is a courtesy to a concurrent writer, not a retry
    /// loop — if the file is still locked after it, the query fails and the
    /// caller gets an error.
    fn open(&self) -> Result<Connection> {
        if !self.db_path.exists() {
            anyhow::bail!(
                "goose sessions database not found: {}",
                self.db_path.display()
            );
        }
        let conn = Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {} read-only", self.db_path.display()))?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "query_only", "ON")?;
        Ok(conn)
    }

    /// `history.sessions {cwd?, q?, limit?}` — summaries, newest first.
    ///
    /// `limit` bounds the SQL; the row count is never left to the caller. Only
    /// the mapped columns leave the database.
    pub fn sessions(&self, params: &Value) -> Result<Value> {
        let cwd = params.get("cwd").and_then(Value::as_str);
        let query = params.get("q").and_then(Value::as_str);
        let limit = limit_param(params, DEFAULT_SESSIONS_LIMIT);
        let conn = self.open()?;
        let mut statement = conn.prepare(
            "SELECT s.id, s.name, s.working_dir, s.created_at, s.updated_at, \
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id) AS message_count, \
                    COALESCE(u.tokens, 0) AS tokens, \
                    COALESCE(u.cost, 0.0) AS cost \
             FROM sessions s \
             LEFT JOIN (SELECT session_id, SUM(total_tokens) AS tokens, SUM(cost) AS cost \
                        FROM usage_ledger GROUP BY session_id) u ON u.session_id = s.id \
             WHERE (?1 IS NULL OR s.working_dir = ?1) \
               AND (?2 IS NULL OR instr(lower(s.name), lower(?2)) > 0) \
             ORDER BY s.updated_at DESC, s.id DESC \
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            rusqlite::params![cwd, query, limit as i64],
            session_from_row,
        )?;
        let sessions: Vec<Value> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "sessions": sessions, "nextCursor": Value::Null }))
    }

    /// `history.session {id}` — one session's metadata and its transcript.
    ///
    /// `history.messages` is the paged way to read a transcript; this returns one
    /// session's messages whole because that is what the hub's `history.session`
    /// does, and a single session is a bounded thing.
    pub fn session(&self, params: &Value) -> Result<Value> {
        let id = params.get("id").and_then(Value::as_str).unwrap_or("");
        let conn = self.open()?;
        let mut statement = conn.prepare(
            "SELECT s.id, s.name, s.working_dir, s.created_at, s.updated_at, \
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id) AS message_count, \
                    COALESCE(u.tokens, 0) AS tokens, \
                    COALESCE(u.cost, 0.0) AS cost \
             FROM sessions s \
             LEFT JOIN (SELECT session_id, SUM(total_tokens) AS tokens, SUM(cost) AS cost \
                        FROM usage_ledger GROUP BY session_id) u ON u.session_id = s.id \
             WHERE s.id = ?1",
        )?;
        let found = statement
            .query_row(rusqlite::params![id], session_from_row)
            .optional()?;
        // The hub distinguishes "no such session" (`session: null`) from an
        // error; a miss is not a failure.
        let Some(session) = found else {
            return Ok(json!({ "session": Value::Null }));
        };
        let messages = self.messages_for(&conn, id, 0, u64::MAX)?;
        Ok(json!({ "session": session, "messages": messages.0 }))
    }

    /// `history.messages {id?, cursor?, limit?}` — one page of a transcript.
    ///
    /// Pagination is done **in the query** (`ORDER BY id LIMIT/OFFSET`): the
    /// cursor is the row offset as an opaque decimal string, and a page never
    /// fetches more than one row past `limit` to learn whether there is more.
    pub fn messages(&self, params: &Value) -> Result<Value> {
        let id = params.get("id").and_then(Value::as_str).unwrap_or("");
        let limit = limit_param(params, DEFAULT_MESSAGES_LIMIT);
        let offset = params
            .get("cursor")
            .and_then(Value::as_str)
            .and_then(|cursor| cursor.parse::<u64>().ok())
            .unwrap_or(0);
        if id.is_empty() {
            return Ok(json!({ "sessionId": id, "messages": [], "nextCursor": Value::Null }));
        }
        let conn = self.open()?;
        let (messages, next_cursor) = self.messages_for(&conn, id, offset, limit)?;
        Ok(json!({
            "sessionId": id,
            "messages": messages,
            "nextCursor": next_cursor.map_or(Value::Null, Value::String),
        }))
    }

    /// `history.search {q?, limit?}` — matches across sessions, grouped by
    /// session.
    ///
    /// An empty query is an empty result, never a scan of everything: the hub's
    /// fake agent defines it that way and the alternative is a full table dump
    /// for a query that asked for nothing.
    pub fn search(&self, params: &Value) -> Result<Value> {
        let query = params.get("q").and_then(Value::as_str).unwrap_or("");
        if query.is_empty() {
            return Ok(json!({ "matches": [] }));
        }
        let limit = limit_param(params, DEFAULT_SEARCH_LIMIT) as usize;
        if limit == 0 {
            return Ok(json!({ "matches": [] }));
        }
        let conn = self.open()?;
        // `content_json LIKE` is only a prefilter — it can match a tool block
        // that has no human-readable text. The authoritative test is the
        // extracted text, below. The scan is bounded: at most
        // `SEARCH_SCAN_PER_SESSION` rows per session we intend to return.
        let scan_cap = (limit as u64).saturating_mul(SEARCH_SCAN_PER_SESSION);
        let mut statement = conn.prepare(
            "SELECT m.session_id, s.name, m.role, m.content_json, m.created_timestamp \
             FROM messages m JOIN sessions s ON s.id = m.session_id \
             WHERE lower(m.content_json) LIKE ?1 ESCAPE '\\' \
             ORDER BY m.id DESC \
             LIMIT ?2",
        )?;
        let pattern = format!("%{}%", escape_like(&query.to_lowercase()));
        let rows = statement.query_map(rusqlite::params![pattern, scan_cap as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;

        let needle = query.to_lowercase();
        // Preserve first-seen session order, and order each session's matches
        // oldest-first within the entry.
        let mut order: Vec<String> = Vec::new();
        let mut grouped: std::collections::HashMap<String, (String, Vec<Value>)> =
            std::collections::HashMap::new();
        for row in rows {
            let (session_id, name, role, content_json, created_timestamp) = row?;
            let text = extract_text(&content_json);
            if !text.to_lowercase().contains(&needle) {
                continue;
            }
            let message = message_value(&role, created_timestamp, &text, 0);
            let entry = grouped.entry(session_id.clone()).or_insert_with(|| {
                order.push(session_id.clone());
                (name, Vec::new())
            });
            entry.1.push(message);
        }
        let matches: Vec<Value> = order
            .into_iter()
            .take(limit)
            .map(|session_id| {
                let (name, mut found) = grouped.remove(&session_id).expect("grouped above");
                found.reverse(); // the SQL walked newest-first; show oldest-first
                json!({ "sessionId": session_id, "name": name, "matches": found })
            })
            .collect();
        Ok(json!({ "matches": matches }))
    }

    /// One page of a session's messages, oldest first.
    ///
    /// Returns the page and, when the query had a row past `limit`, the cursor
    /// for the next page. `limit == u64::MAX` means "all of it" for
    /// `history.session`; that path passes `LIMIT -1` and never asks for a
    /// cursor.
    fn messages_for(
        &self,
        conn: &Connection,
        session_id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let fetch_all = limit == u64::MAX;
        let sql_limit: i64 = if fetch_all {
            -1
        } else {
            // One past the page, to learn whether a next page exists.
            (limit + 1) as i64
        };
        let mut statement = conn.prepare(
            "SELECT role, content_json, created_timestamp FROM messages \
             WHERE session_id = ?1 ORDER BY id ASC LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(
            rusqlite::params![session_id, sql_limit, offset as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        let mut messages = Vec::new();
        let mut has_more = false;
        for row in rows {
            let (role, content_json, created_timestamp) = row?;
            if !fetch_all && messages.len() as u64 == limit {
                has_more = true;
                break;
            }
            let index = offset as usize + messages.len();
            messages.push(message_value(
                &role,
                created_timestamp,
                &extract_text(&content_json),
                index,
            ));
        }
        let next_cursor = has_more.then(|| (offset + messages.len() as u64).to_string());
        Ok((messages, next_cursor))
    }
}

/// Map a `sessions` row onto the hub's session object.
fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "sessionId": row.get::<_, String>(0)?,
        "name": row.get::<_, String>(1)?,
        "workingDir": row.get::<_, String>(2)?,
        // goose stores these as its own timestamp strings; passed through
        // verbatim so the hub sees exactly what goose wrote.
        "createdAt": row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        "updatedAt": row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        "messageCount": row.get::<_, i64>(5)?,
        // tokens/cost come from `usage_ledger` — goose's own accounting —
        // summed per session. A session with no ledger rows is honestly 0/0.0:
        // "nothing recorded", not an invented number.
        "tokens": row.get::<_, i64>(6)?,
        "cost": row.get::<_, f64>(7)?,
    }))
}

/// Map one message onto the hub's message object.
fn message_value(role: &str, created_timestamp: i64, text: &str, index: usize) -> Value {
    json!({
        "index": index,
        "role": role,
        "createdAt": rfc3339(created_timestamp),
        "text": text,
    })
}

/// The human-readable text of a goose message body.
///
/// goose stores `content_json` as a JSON array of content blocks
/// (`{type: "text"|"thinking"|"toolRequest"|"toolResponse", ...}`). We take the
/// `text` of every block that has one and join them. Anything unexpected — not
/// JSON, not an array, a block that is not an object — yields an empty string
/// for that input rather than an error: the transcript is a *view*, and a blob
/// this build does not understand must not fail the query or panic.
pub fn extract_text(content_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(content_json) else {
        // Not JSON at all. If it is a bare string, that *is* the text; otherwise
        // say nothing rather than dump an opaque blob into the UI.
        let trimmed = content_json.trim();
        if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
            return serde_json::from_str::<String>(trimmed).unwrap_or_default();
        }
        return String::new();
    };
    match value {
        Value::String(text) => text,
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            parts.join("\n")
        }
        // An object with a `text` field is still recognisable; anything else is
        // an unknown shape and contributes nothing.
        Value::Object(ref map) => map
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Epoch seconds to an RFC 3339 UTC string. An out-of-range value becomes the
/// empty string rather than an error — it is one field on one row.
fn rfc3339(epoch_seconds: i64) -> String {
    chrono::DateTime::from_timestamp(epoch_seconds, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// A caller `limit`, defaulted and clamped. `0` is honoured as "no rows".
fn limit_param(params: &Value, default: u64) -> u64 {
    params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .min(MAX_LIMIT)
}

/// Escape the LIKE metacharacters so a search for `100%` is a search for the
/// literal text, not a wildcard.
fn escape_like(needle: &str) -> String {
    let mut escaped = String::with_capacity(needle.len());
    for ch in needle.chars() {
        match ch {
            '\\' | '%' | '_' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fixture database built with goose's real column names, in a fresh temp
    /// directory. No test here touches `~/.local/share/goose`.
    struct Fixture {
        dir: PathBuf,
        store: HistoryStore,
        writer: Connection,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "a2a-goose-history-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let db_path = dir.join("sessions.db");
            let writer = Connection::open(&db_path).expect("open fixture");
            writer
                .execute_batch(
                    "CREATE TABLE sessions (
                        id TEXT PRIMARY KEY,
                        name TEXT NOT NULL DEFAULT '',
                        working_dir TEXT NOT NULL,
                        created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                        updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                        total_tokens INTEGER,
                        accumulated_total_tokens INTEGER,
                        accumulated_cost REAL
                    );
                    CREATE TABLE messages (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        message_id TEXT,
                        session_id TEXT NOT NULL REFERENCES sessions(id),
                        role TEXT NOT NULL,
                        content_json TEXT NOT NULL,
                        created_timestamp INTEGER NOT NULL,
                        tokens INTEGER,
                        metadata_json TEXT
                    );
                    CREATE TABLE usage_ledger (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        session_id TEXT NOT NULL REFERENCES sessions(id),
                        created_timestamp INTEGER NOT NULL,
                        model TEXT,
                        input_tokens INTEGER,
                        output_tokens INTEGER,
                        total_tokens INTEGER,
                        cost REAL
                    );",
                )
                .expect("schema");
            Self {
                dir,
                store: HistoryStore::new(&db_path),
                writer,
            }
        }

        fn session(&self, id: &str, name: &str, cwd: &str, updated: &str) {
            self.writer
                .execute(
                    "INSERT INTO sessions (id, name, working_dir, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?4)",
                    rusqlite::params![id, name, cwd, updated],
                )
                .expect("insert session");
        }

        fn message(&self, session_id: &str, role: &str, content_json: &str, at: i64) {
            self.writer
                .execute(
                    "INSERT INTO messages (session_id, role, content_json, created_timestamp) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![session_id, role, content_json, at],
                )
                .expect("insert message");
        }

        fn ledger(&self, session_id: &str, total_tokens: i64, cost: f64) {
            self.writer
                .execute(
                    "INSERT INTO usage_ledger (session_id, created_timestamp, total_tokens, cost) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![session_id, 0i64, total_tokens, cost],
                )
                .expect("insert ledger");
        }

        fn text_message(&self, session_id: &str, role: &str, text: &str, at: i64) {
            let content = json!([{ "type": "text", "text": text }]).to_string();
            self.message(session_id, role, &content, at);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn populated() -> Fixture {
        let fixture = Fixture::new();
        fixture.session(
            "s1",
            "Scaffold the roost hub",
            "/workspaces/roost",
            "2026-09-20 02:00:00",
        );
        fixture.session(
            "s2",
            "Fix the docker publish plugin",
            "/workspaces/roost",
            "2026-09-21 12:00:00",
        );
        fixture.session(
            "s3",
            "Review the mission control memo",
            "/workspaces/a2a-goose",
            "2026-09-19 09:00:00",
        );
        fixture.ledger("s1", 18_432, 0.42);
        fixture.ledger("s1", 1_000, 0.01);
        fixture.ledger("s3", 33_210, 0.77);
        fixture.text_message("s1", "user", "Add a fake agent to roost.", 1_000);
        fixture.text_message("s1", "assistant", "Starting with the wire protocol.", 1_001);
        fixture.text_message("s1", "assistant", "The hub registers agents.", 1_002);
        fixture.text_message(
            "s2",
            "user",
            "The publish step dies with --bootstrap.",
            2_000,
        );
        fixture.text_message("s3", "user", "Read the mission control memo.", 3_000);
        // A message whose body is an unexpected blob: it must not panic and
        // must not be searchable as garbage.
        fixture.message("s3", "assistant", "{not json at all", 3_001);
        fixture
    }

    #[test]
    fn sessions_are_newest_first_and_carry_ledger_tokens_and_cost() {
        let fixture = populated();
        let body = fixture.store.sessions(&json!({})).expect("sessions");
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0]["sessionId"], "s2"); // 2026-09-21 is newest
        assert_eq!(sessions[1]["sessionId"], "s1");
        assert_eq!(sessions[2]["sessionId"], "s3");
        assert!(body["nextCursor"].is_null());

        let s1 = &sessions[1];
        assert_eq!(s1["name"], "Scaffold the roost hub");
        assert_eq!(s1["workingDir"], "/workspaces/roost");
        assert_eq!(s1["messageCount"], 3);
        // usage_ledger is summed per session: 18432 + 1000 tokens, 0.42 + 0.01.
        assert_eq!(s1["tokens"], 19_432);
        assert!((s1["cost"].as_f64().unwrap() - 0.43).abs() < 1e-9);
        // s2 has no ledger rows, which is honestly 0 / 0.0 — not an invention.
        assert_eq!(sessions[0]["tokens"], 0);
        assert_eq!(sessions[0]["cost"], 0.0);
    }

    #[test]
    fn sessions_filter_on_cwd_and_name_and_bound_the_limit() {
        let fixture = populated();
        let by_cwd = fixture
            .store
            .sessions(&json!({ "cwd": "/workspaces/roost" }))
            .expect("cwd");
        assert_eq!(by_cwd["sessions"].as_array().unwrap().len(), 2);

        let by_name = fixture
            .store
            .sessions(&json!({ "q": "DOCKER" }))
            .expect("q");
        assert_eq!(by_name["sessions"][0]["sessionId"], "s2");

        let limited = fixture
            .store
            .sessions(&json!({ "limit": 1 }))
            .expect("limit");
        assert_eq!(limited["sessions"].as_array().unwrap().len(), 1);

        let none = fixture
            .store
            .sessions(&json!({ "q": "nothing matches" }))
            .expect("empty");
        assert!(none["sessions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn session_returns_metadata_and_transcript_or_null() {
        let fixture = populated();
        let body = fixture
            .store
            .session(&json!({ "id": "s1" }))
            .expect("session");
        assert_eq!(body["session"]["sessionId"], "s1");
        assert_eq!(body["session"]["messageCount"], 3);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["index"], 0);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["text"], "Add a fake agent to roost.");
        assert_eq!(messages[0]["createdAt"], "1970-01-01T00:16:40Z");

        let missing = fixture
            .store
            .session(&json!({ "id": "nope" }))
            .expect("missing");
        assert!(missing["session"].is_null());
        assert!(missing.get("messages").is_none());
    }

    #[test]
    fn messages_paginate_with_an_opaque_cursor() {
        let fixture = populated();
        let first = fixture
            .store
            .messages(&json!({ "id": "s1", "limit": 2 }))
            .expect("first page");
        assert_eq!(first["sessionId"], "s1");
        let page = first["messages"].as_array().unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0]["index"], 0);
        assert_eq!(page[1]["index"], 1);
        assert_eq!(first["nextCursor"], "2");

        let second = fixture
            .store
            .messages(&json!({ "id": "s1", "cursor": "2", "limit": 2 }))
            .expect("second page");
        let page = second["messages"].as_array().unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0]["index"], 2);
        assert!(second["nextCursor"].is_null());

        // An unknown id is an empty page, not an error.
        let unknown = fixture
            .store
            .messages(&json!({ "id": "nope" }))
            .expect("unknown");
        assert!(unknown["messages"].as_array().unwrap().is_empty());

        // A defaulted limit matches the hub's fake agent (2).
        let defaulted = fixture
            .store
            .messages(&json!({ "id": "s1" }))
            .expect("default");
        assert_eq!(defaulted["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn search_groups_by_session_and_an_empty_query_is_empty() {
        let fixture = populated();
        let found = fixture
            .store
            .search(&json!({ "q": "roost" }))
            .expect("search");
        let matches = found["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["sessionId"], "s1");
        assert_eq!(matches[0]["name"], "Scaffold the roost hub");
        assert_eq!(
            matches[0]["matches"].as_array().unwrap()[0]["text"],
            "Add a fake agent to roost."
        );

        assert!(
            fixture.store.search(&json!({ "q": "" })).expect("empty q")["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        assert!(
            fixture
                .store
                .search(&json!({ "q": "zzz-not-there" }))
                .expect("no match")["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        // LIKE metacharacters in the query are literal, not wildcards.
        assert!(
            fixture
                .store
                .search(&json!({ "q": "%" }))
                .expect("literal percent")["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn text_extraction_is_defensive() {
        assert_eq!(
            extract_text(r#"[{"type":"text","text":"hello"},{"type":"text","text":"world"}]"#),
            "hello\nworld"
        );
        // A bare JSON string is the text.
        assert_eq!(extract_text("\"just a string\""), "just a string");
        // Unknown blob: empty, never a panic or a serde error.
        assert_eq!(extract_text("{not json"), "");
        assert_eq!(extract_text(""), "");
        assert_eq!(extract_text("null"), "");
        assert_eq!(extract_text("[]"), "");
        assert_eq!(extract_text("[1,2,3]"), "");
        assert_eq!(extract_text(r#"[{"type":"toolRequest"}]"#), "");
    }

    #[test]
    fn a_missing_database_is_an_error_for_that_query_only() {
        let store = HistoryStore::new("/nonexistent/a2a-goose/no-such-sessions.db");
        let error = store.sessions(&json!({})).expect_err("missing db");
        assert!(error.to_string().contains("not found"), "{error}");
        // The other queries fail the same way, and none of them panics.
        assert!(store.messages(&json!({ "id": "x" })).is_err());
        assert!(store.search(&json!({ "q": "x" })).is_err());
    }

    #[test]
    fn a_locked_database_is_an_error_not_a_hang_or_a_write() {
        let mut fixture = Fixture::new();
        fixture.session("s1", "held", "/tmp", "2026-09-20 02:00:00");
        // Take an EXCLUSIVE lock from a separate connection and hold it.
        let locker = Connection::open(fixture.dir.join("sessions.db")).expect("locker");
        locker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("take exclusive lock");
        let error = fixture.store.sessions(&json!({})).expect_err("locked db");
        assert!(
            error.to_string().contains("locked") || error.to_string().contains("busy"),
            "{error}"
        );
        locker.execute_batch("COMMIT").expect("release lock");
        // Once released, the same store answers again: a lock cost one query,
        // not the connection.
        assert!(fixture.store.sessions(&json!({})).is_ok());
        let _ = &mut fixture.writer;
    }

    #[test]
    fn the_connection_is_actually_read_only() {
        let fixture = populated();
        let conn = fixture.store.open().expect("open");
        let write = conn.execute(
            "INSERT INTO sessions (id, working_dir) VALUES ('x', '/x')",
            [],
        );
        assert!(write.is_err(), "a read-only connection must refuse writes");
    }

    #[test]
    fn an_unknown_method_is_an_error_not_an_empty_body() {
        let fixture = populated();
        assert!(
            fixture
                .store
                .answer("history.from_the_future", &json!({}))
                .is_err()
        );
    }
}
