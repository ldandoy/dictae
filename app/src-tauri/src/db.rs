//! Historique local des dictées (SQLite).

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::vocab::{VocabEntry, VocabKind};

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
             CREATE INDEX IF NOT EXISTS history_created_at ON history(created_at DESC);
             CREATE TABLE IF NOT EXISTS vocabulary (
                 id          TEXT PRIMARY KEY,
                 kind        TEXT NOT NULL,
                 phrase      TEXT NOT NULL,
                 target      TEXT NOT NULL DEFAULT '',
                 note        TEXT NOT NULL DEFAULT '',
                 uses        INTEGER NOT NULL DEFAULT 0,
                 created_at  TEXT NOT NULL,
                 updated_at  TEXT NOT NULL,
                 UNIQUE(kind, phrase COLLATE NOCASE)
             );",
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

    // ----- vocabulaire -------------------------------------------------------

    pub fn vocab_list(&self) -> Result<Vec<VocabEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, phrase, target, note, uses, created_at, updated_at
             FROM vocabulary ORDER BY kind, phrase COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            let kind: String = r.get(1)?;
            Ok(VocabEntry {
                id: r.get(0)?,
                kind: VocabKind::parse(&kind).unwrap_or(VocabKind::Term),
                phrase: r.get(2)?,
                target: r.get(3)?,
                note: r.get(4)?,
                uses: r.get(5)?,
                created_at: r.get(6)?,
                updated_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Crée ou met à jour une entrée (par identifiant). Refuse un doublon de phrase
    /// pour le même type.
    pub fn vocab_upsert(&self, e: &VocabEntry) -> Result<()> {
        let phrase = e.phrase.trim();
        if phrase.is_empty() {
            return Err(anyhow!("la phrase est vide"));
        }
        if matches!(e.kind, VocabKind::Replacement | VocabKind::Snippet) && e.target.trim().is_empty() {
            return Err(anyhow!("le texte de remplacement est vide"));
        }
        let conn = self.conn.lock();
        let duplicate: Option<String> = conn
            .query_row(
                "SELECT id FROM vocabulary WHERE kind = ?1 AND phrase = ?2 COLLATE NOCASE AND id <> ?3",
                params![e.kind.as_str(), phrase, e.id],
                |r| r.get(0),
            )
            .ok();
        if duplicate.is_some() {
            return Err(anyhow!("« {phrase} » existe déjà dans cette catégorie"));
        }
        conn.execute(
            "INSERT INTO vocabulary (id, kind, phrase, target, note, uses, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, phrase = excluded.phrase,
                 target = excluded.target, note = excluded.note, updated_at = excluded.updated_at",
            params![
                e.id,
                e.kind.as_str(),
                phrase,
                e.target,
                e.note,
                e.uses,
                e.created_at,
                e.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn vocab_delete(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM vocabulary WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Incrémente le compteur d'utilisation des entrées données.
    pub fn vocab_bump(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("UPDATE vocabulary SET uses = uses + 1 WHERE id = ?1")?;
        for id in ids {
            stmt.execute(params![id])?;
        }
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
    fn vocabulary_crud_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("v.sqlite")).unwrap();
        let mk = |id: &str, kind: VocabKind, phrase: &str, target: &str| VocabEntry {
            id: id.into(),
            kind,
            phrase: phrase.into(),
            target: target.into(),
            note: String::new(),
            uses: 0,
            created_at: "2026-10-24T00:00:00Z".into(),
            updated_at: "2026-10-24T00:00:00Z".into(),
        };
        db.vocab_upsert(&mk("1", VocabKind::Term, "Dictae", "")).unwrap();
        db.vocab_upsert(&mk("2", VocabKind::Replacement, "dicté", "Dictae")).unwrap();
        assert!(db.vocab_upsert(&mk("3", VocabKind::Term, "dictae", "")).is_err(), "doublon insensible à la casse");
        assert!(db.vocab_upsert(&mk("4", VocabKind::Snippet, "sig", "")).is_err(), "snippet sans texte");
        assert!(db.vocab_upsert(&mk("5", VocabKind::Term, "  ", "")).is_err());
        // mise à jour par id
        db.vocab_upsert(&mk("2", VocabKind::Replacement, "dicté", "DICTAE")).unwrap();
        let list = db.vocab_list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.iter().find(|e| e.id == "2").unwrap().target, "DICTAE");
        db.vocab_bump(&["2".into(), "2".into()]).unwrap();
        assert_eq!(db.vocab_list().unwrap().iter().find(|e| e.id == "2").unwrap().uses, 2);
        db.vocab_delete("1").unwrap();
        assert_eq!(db.vocab_list().unwrap().len(), 1);
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
