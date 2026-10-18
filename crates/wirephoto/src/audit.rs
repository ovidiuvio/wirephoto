//! The audit trail: every change made through the API (uploads, folders, copies, moves, deletes, restores), every
//! refused request, every key created or revoked, and what the server deletes itself (the trash it empties).
//! Each is a line in the log (`wirephoto::audit`) and a row in the index, kept `[log] audit_days`
//! (`wirephoto audit` lists them). Plain requests are the access log's (`wirephoto::access`), not this.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

use rusqlite::params;

use crate::app::App;
use crate::db::Db;

/// Who is asking, set for every request by the access log and filled in by authentication: what the audit
/// trail records as the actor.
#[derive(Clone, Debug, Default)]
pub struct RequestInfo {
    pub client: Option<IpAddr>,
    /// The key's name, once authenticated.
    pub key: Arc<OnceLock<String>>,
}

impl RequestInfo {
    pub fn key_name(&self) -> Option<&str> {
        self.key.get().map(String::as_str)
    }
}

/// One audited operation.
#[derive(Debug, Default)]
pub struct Event<'a> {
    /// `upload`, `create-folder`, `copy`, `move`, `delete`, `restore`, `denied`, `key-create`, `key-revoke`,
    /// `purge`.
    pub op: &'a str,
    pub key: Option<&'a str>,
    pub client: Option<String>,
    pub entry: Option<&'a str>,
    pub path: Option<&'a str>,
    pub detail: String,
    pub status: u16,
    pub ms: u64,
}

impl<'a> Event<'a> {
    pub fn new(op: &'a str, detail: impl Into<String>) -> Self {
        Event {
            op,
            detail: detail.into(),
            status: 200,
            ..Default::default()
        }
    }

    /// The actor, from the request.
    pub fn by(mut self, info: Option<&'a RequestInfo>, key: Option<&'a str>) -> Self {
        self.key = key.or_else(|| info.and_then(|i| i.key_name()));
        self.client = info.and_then(|i| i.client).map(|ip| ip.to_string());
        self
    }

    pub fn entry(mut self, id: &'a str, path: &'a str) -> Self {
        self.entry = Some(id);
        self.path = Some(path);
        self
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn took(mut self, started: std::time::Instant) -> Self {
        self.ms = started.elapsed().as_millis() as u64;
        self
    }
}

/// Logs and stores an event. Never fails the operation it describes (a write error is logged).
pub fn record(app: &App, e: Event) {
    record_in(&app.db, e)
}

/// The same, with only the index at hand (the `keys` command).
pub fn record_in(db: &Db, e: Event) {
    tracing::info!(
        target: "wirephoto::audit",
        op = e.op,
        key = e.key.unwrap_or("-"),
        client = e.client.as_deref().unwrap_or("-"),
        path = e.path.unwrap_or("-"),
        status = e.status,
        ms = e.ms,
        "{}",
        e.detail
    );
    let res = db.write(|t| {
        t.execute(
            "INSERT INTO audit (time, op, key, client, entry, path, detail, status, ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![crate::util::now_secs(), e.op, e.key, e.client, e.entry, e.path, e.detail, e.status, e.ms as i64],
        )?;
        Ok(())
    });
    if let Err(err) = res {
        tracing::warn!("audit trail not written: {err:#}");
    }
}

/// Drops rows older than `days` (0 keeps everything).
pub fn prune(app: &App, days: u32) -> anyhow::Result<usize> {
    if days == 0 {
        return Ok(0);
    }
    let cutoff = crate::util::now_secs() - days as i64 * 86400;
    app.db
        .write(|t| Ok(t.execute("DELETE FROM audit WHERE time < ?", [cutoff])?))
}

/// A stored event, for `wirephoto audit`.
#[derive(Debug)]
pub struct Row {
    pub time: i64,
    pub op: String,
    pub key: Option<String>,
    pub client: Option<String>,
    pub path: Option<String>,
    pub detail: String,
    pub status: u16,
    pub ms: u64,
}

/// Events since `since` (unix seconds), newest last, optionally of one key or op.
pub fn list(
    db: &Db,
    since: i64,
    key: Option<&str>,
    op: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<Row>> {
    db.read(|c| {
        let mut st = c.prepare(
            "SELECT time, op, key, client, path, detail, status, ms FROM audit
             WHERE time >= ?1 AND (?2 IS NULL OR key = ?2) AND (?3 IS NULL OR op = ?3)
             ORDER BY id DESC LIMIT ?4",
        )?;
        let mut rows = st
            .query_map(params![since, key, op, limit as i64], |r| {
                Ok(Row {
                    time: r.get(0)?,
                    op: r.get(1)?,
                    key: r.get(2)?,
                    client: r.get(3)?,
                    path: r.get(4)?,
                    detail: r.get(5)?,
                    status: r.get::<_, i64>(6)? as u16,
                    ms: r.get::<_, i64>(7)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.reverse();
        Ok(rows)
    })
}
