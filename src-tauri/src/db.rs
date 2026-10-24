//! Historique local des dictées (SQLite).

use std::path::Path;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub created_at: String,
    /// Transcription brute (sortie du moteur de reconnaissance).
    pub asr_text: String,
    /// Texte après mise en forme par le modèle de langue, s'il diffère du brut.
    pub formatted_text: Option<String>,
    /// Texte effectivement collé.
    pub pasted_text: Option<String>,
    /// `ok`, `empty`, `paste_failed`, `asr_failed`
    pub status: String,
    pub duration_ms: i64,
    pub latency_ms: i64,
    pub model: String,
    pub error: Option<String>,
    /// Application cible, titre de fenêtre et URL (navigateur) au moment de la dictée.
    pub app: Option<String>,
    pub window_title: Option<String>,
    pub url: Option<String>,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("création du dossier de données")?;
        }
        let conn = Connection::open(path).context("ouverture de la base SQLite")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS history (
                 id             TEXT PRIMARY KEY,
                 created_at     TEXT NOT NULL,
                 asr_text       TEXT NOT NULL DEFAULT '',
                 formatted_text TEXT,
                 pasted_text    TEXT,
                 status         TEXT NOT NULL,
                 duration_ms    INTEGER NOT NULL DEFAULT 0,
                 latency_ms     INTEGER NOT NULL DEFAULT 0,
                 model          TEXT NOT NULL DEFAULT '',
                 error          TEXT,
                 app            TEXT,
                 window_title   TEXT,
                 url            TEXT
             );
             CREATE INDEX IF NOT EXISTS history_created_at ON history(created_at DESC);",
        )
        .context("initialisation du schéma")?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, e: &HistoryEntry) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO history (id, created_at, asr_text, formatted_text, pasted_text, status, duration_ms, latency_ms, model, error, app, window_title, url)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                e.id,
                e.created_at,
                e.asr_text,
                e.formatted_text,
                e.pasted_text,
                e.status,
                e.duration_ms,
                e.latency_ms,
                e.model,
                e.error,
                e.app,
                e.window_title,
                e.url
            ],
        )?;
        Ok(())
    }

    pub fn list(&self, limit: u32) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, created_at, asr_text, formatted_text, pasted_text, status, duration_ms, latency_ms, model, error, app, window_title, url
             FROM history ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                created_at: r.get(1)?,
                asr_text: r.get(2)?,
                formatted_text: r.get(3)?,
                pasted_text: r.get(4)?,
                status: r.get(5)?,
                duration_ms: r.get(6)?,
                latency_ms: r.get(7)?,
                model: r.get(8)?,
                error: r.get(9)?,
                app: r.get(10)?,
                window_title: r.get(11)?,
                url: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM history WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        self.conn.lock().execute("DELETE FROM history", [])?;
        Ok(())
    }
}

/// Ajoute les colonnes apparues après la première version du schéma.
fn migrate(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(history)")?;
    let columns: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    for column in ["formatted_text", "app", "window_title", "url"] {
        if !columns.iter().any(|c| c == column) {
            conn.execute(&format!("ALTER TABLE history ADD COLUMN {column} TEXT"), [])
                .with_context(|| format!("migration : colonne {column}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, created_at: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            created_at: created_at.into(),
            asr_text: format!("texte {id}"),
            formatted_text: Some(format!("Texte {id}.")),
            pasted_text: None,
            status: "ok".into(),
            duration_ms: 1200,
            latency_ms: 800,
            model: "m".into(),
            error: None,
            app: Some("Slack".into()),
            window_title: None,
            url: None,
        }
    }

    #[test]
    fn insert_list_delete_clear() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("data").join("t.sqlite")).unwrap();
        db.insert(&entry("a", "2026-10-24T10:00:00Z")).unwrap();
        db.insert(&entry("b", "2026-10-24T11:00:00Z")).unwrap();
        let list = db.list(10).unwrap();
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["b", "a"], "plus récent d'abord");
        assert_eq!(list[0].formatted_text.as_deref(), Some("Texte b."));
        assert_eq!(list[0].app.as_deref(), Some("Slack"));
        assert_eq!(db.list(1).unwrap().len(), 1);
        db.delete("b").unwrap();
        assert_eq!(db.list(10).unwrap()[0].id, "a");
        db.clear().unwrap();
        assert!(db.list(10).unwrap().is_empty());
    }

    #[test]
    fn migrates_old_schema_without_formatted_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE history (id TEXT PRIMARY KEY, created_at TEXT NOT NULL, asr_text TEXT NOT NULL DEFAULT '',
                 pasted_text TEXT, status TEXT NOT NULL, duration_ms INTEGER NOT NULL DEFAULT 0,
                 latency_ms INTEGER NOT NULL DEFAULT 0, model TEXT NOT NULL DEFAULT '', error TEXT);
                 INSERT INTO history (id, created_at, status) VALUES ('x', '2026-01-01T00:00:00Z', 'ok');",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let list = db.list(10).unwrap();
        assert_eq!(list[0].id, "x");
        assert_eq!(list[0].formatted_text, None);
        assert_eq!(list[0].app, None);
        db.insert(&entry("y", "2026-01-02T00:00:00Z")).unwrap();
        assert_eq!(db.list(10).unwrap().len(), 2);
    }
}
