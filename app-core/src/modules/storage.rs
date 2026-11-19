use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Returns true when `dir` looks like a Cargo build output directory
/// (`.../target/debug`, `.../target/release`, or their `/deps` subdirectories).
pub fn is_cargo_target_dir(dir: &Path) -> bool {
    let comps: Vec<String> = dir
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    comps.windows(2).any(|w| {
        w[0] == "target" && matches!(w[1].as_str(), "debug" | "release")
    }) || comps.windows(3).any(|w| {
        w[0] == "target" && matches!(w[2].as_str(), "debug" | "release")
    })
}

/// Resolves the storage directory cleanly:
/// 1. `MINI_DATA_DIR` environment override (tests / custom deployments).
/// 2. Portable mode (`<exe_dir>/data`): active whenever `<exe_dir>/data` exists
///    OR the executable is running outside a Cargo `target/` directory.
/// 3. Development fallback (`%APPDATA%/MiniAssistant`) when running `cargo run` / `cargo test`.
pub fn resolve_data_dir_for_exe(exe_dir: Option<&Path>, env_override: Option<&str>) -> PathBuf {
    if let Some(custom) = env_override.map(str::trim).filter(|s| !s.is_empty()) {
        return PathBuf::from(custom);
    }
    if let Some(dir) = exe_dir {
        let portable_dir = dir.join("data");
        if portable_dir.is_dir() || !is_cargo_target_dir(dir) {
            return portable_dir;
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MiniAssistant")
}

/// Active data directory for the running process.
pub fn resolve_data_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let env_override = std::env::var("MINI_DATA_DIR").ok();
    resolve_data_dir_for_exe(exe_dir.as_deref(), env_override.as_deref())
}

/// True when the running process stores its database, accounts, and cache
/// alongside the executable (`<exe_dir>/data`).
pub fn is_portable_mode() -> bool {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    match exe_dir {
        Some(dir) => dir.join("data").is_dir() || !is_cargo_target_dir(&dir),
        None => false,
    }
}

/// Portable-aware path to `accounts.json` (`<data_dir>/accounts.json`).
pub fn portable_accounts_path() -> PathBuf {
    resolve_data_dir().join("accounts.json")
}

/// Portable-aware temporary screenshot cache directory (`<data_dir>/cache`).
pub fn portable_cache_dir() -> PathBuf {
    resolve_data_dir().join("cache")
}

/// Structured payload for inserting a chat message row into SQLite.
pub struct MessageRecord<'a> {
    pub topic_name: &'a str,
    pub category: &'a str,
    pub role: &'a str,
    pub content: &'a str,
    pub image_url: Option<&'a str>,
    pub thought: &'a str,
    pub meta: &'a str,
    pub sources: &'a [StoredSearchSource],
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredSearchSource {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub site_name: String,
    pub date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub topic_name: String,
    pub role: String, // "user" | "assistant"
    pub content: String,
    pub image_url: Option<String>,
    pub created_at: String,
    pub thought: String,
    pub meta: String,
    pub sources: Vec<StoredSearchSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicItem {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub created_at: String,
}

/// Stored topic label for a detected window title.
///
/// A conversation about the assistant itself would be titled after the HUD
/// window, which reads like a bug in the history list, so self-references
/// collapse to "General Desktop". The UI runs every window-derived name
/// through this before showing it, so the pill and the stored row agree.
pub fn topic_label(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("mini assistant")
        || lower.contains("mini assisten")
        || lower.contains("mini-assistant")
        || lower.contains("mini_assistant")
        || lower.contains("sidequest")
        || lower.contains("aemeath")
        || lower.contains("aemeth")
    {
        "General Desktop".to_string()
    } else {
        raw.to_string()
    }
}

pub struct StorageManager {
    db_path: PathBuf,
}

impl StorageManager {
    pub fn new() -> Result<Self, String> {
        let app_dir = resolve_data_dir();
        let portable = is_portable_mode();

        if !app_dir.exists() {
            let _ = fs::create_dir_all(&app_dir);
            // Migrate legacy AppData database only in non-portable dev mode
            if !portable {
                let config_root = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
                let legacy_db = config_root.join("SideQuestAI").join("sidequest.db");
                let target_db = app_dir.join("mini_assistant.db");
                if legacy_db.exists() && !target_db.exists() {
                    let _ = fs::copy(&legacy_db, &target_db);
                }
            }
        }

        let db_path = app_dir.join("mini_assistant.db");
        let s = Self { db_path };
        s.init_tables()?;
        Ok(s)
    }

    fn get_conn(&self) -> Result<Connection, String> {
        let conn = Connection::open(&self.db_path)
            .map_err(|e| format!("SQLite open error: {}", e))?;
        // The UI thread reads while the worker thread writes the answer; without
        // a busy timeout a short write lock surfaces as a spurious "database is
        // locked" and the message is lost.
        let _ = conn.busy_timeout(std::time::Duration::from_millis(3000));
        Ok(conn)
    }

    fn init_tables(&self) -> Result<(), String> {
        let conn = self.get_conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS topics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                category TEXT NOT NULL,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                topic_id INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                image_url TEXT,
                thought TEXT DEFAULT '',
                meta TEXT DEFAULT '',
                sources_json TEXT DEFAULT '',
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(topic_id) REFERENCES topics(id)
            );
            -- get_messages() filters by topic and orders by id; without this the
            -- whole table is scanned every time a session is opened.
            CREATE INDEX IF NOT EXISTS idx_messages_topic ON messages(topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_topics_created ON topics(created_at DESC);",
        )
        .map_err(|e| format!("Failed to init SQLite tables: {}", e))?;

        ensure_message_columns(&conn)?;

        // WAL keeps the reader (session list on the UI thread) from blocking on
        // the writer (assistant message landing in the background). It is a
        // property of the database file, so setting it once is enough.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");

        // Insert default priority setting if not exists
        let default_priority = r#"["deepseek", "grok", "gemini", "chatgpt", "claude"]"#;
        let _ = conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES ('provider_priority', ?1)",
            params![default_priority],
        );

        // Purge any legacy self-referential topics
        let _ = conn.execute(
            "DELETE FROM topics WHERE LOWER(name) LIKE '%mini assistant%' 
             OR LOWER(name) LIKE '%mini assisten%' 
             OR LOWER(name) LIKE '%mini-assistant%' 
             OR LOWER(name) LIKE '%sidequest%' 
             OR LOWER(name) LIKE '%aemeath%' 
             OR LOWER(name) LIKE '%aemeth%'",
            [],
        );

        Ok(())
    }

    pub fn save_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let conn = self.get_conn()?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) 
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )
        .map_err(|e| format!("Failed to save setting: {}", e))?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, String> {
        let conn = self.get_conn()?;
        let mut stmt = conn
            .prepare("SELECT value FROM settings WHERE key = ?1")
            .map_err(|e| format!("Prepare error: {}", e))?;
        let mut rows = stmt
            .query(params![key])
            .map_err(|e| format!("Query error: {}", e))?;

        if let Some(row) = rows.next().map_err(|e| format!("Row error: {}", e))? {
            let val: String = row.get(0).map_err(|e| format!("Get error: {}", e))?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    pub fn get_topics(&self) -> Result<Vec<TopicItem>, String> {
        let conn = self.get_conn()?;
        let mut stmt = conn
            .prepare("SELECT id, name, category, created_at FROM topics ORDER BY id DESC")
            .map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(TopicItem {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .map_err(|e| format!("Query map error: {}", e))?;

        let mut topics = Vec::new();
        for r in rows.flatten() {
            let lower = r.name.to_lowercase();
            if !lower.contains("mini assistant")
                && !lower.contains("mini assisten")
                && !lower.contains("mini-assistant")
                && !lower.contains("mini_assistant")
                && !lower.contains("sidequest")
                && !lower.contains("aemeath")
                && !lower.contains("aemeth")
            {
                topics.push(r);
            }
        }
        Ok(topics)
    }

    /// Message count per topic in one query — the history page needs it for
    /// every row, and one query per keystroke would not scale.
    pub fn topic_counts(&self) -> Result<Vec<(String, i64)>, String> {
        let conn = self.get_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT t.name, COUNT(m.id) FROM topics t                  LEFT JOIN messages m ON m.topic_id = t.id GROUP BY t.id",
            )
            .map_err(|e| format!("Prepare error: {e}"))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| format!("Query map error: {e}"))?;
        Ok(rows.flatten().collect())
    }

    /// Creates the topic row if it is not there yet (used by "Sesi Baru", which
    /// must be visible in the history list before its first message exists).
    pub fn ensure_topic(&self, name: &str, category: &str) -> Result<(), String> {
        let conn = self.get_conn()?;
        let name = topic_label(name);
        conn.execute(
            "INSERT OR IGNORE INTO topics (name, category) VALUES (?1, ?2)",
            params![&name, category],
        )
        .map(|_| ())
        .map_err(|e| format!("Insert topic error: {e}"))
    }

    pub fn save_message(
        &self,
        topic_name: &str,
        category: &str,
        role: &str,
        content: &str,
        image_url: Option<&str>,
    ) -> Result<(), String> {
        self.save_message_record(&MessageRecord {
            topic_name,
            category,
            role,
            content,
            image_url,
            thought: "",
            meta: "",
            sources: &[],
        })
    }

    pub fn save_message_record(&self, rec: &MessageRecord<'_>) -> Result<(), String> {
        let conn = self.get_conn()?;
        let clean_topic = topic_label(rec.topic_name);

        // Ensure topic exists
        let _ = conn.execute(
            "INSERT OR IGNORE INTO topics (name, category) VALUES (?1, ?2)",
            params![&clean_topic, rec.category],
        );

        let topic_id: i64 = conn
            .query_row(
                "SELECT id FROM topics WHERE name = ?1",
                params![&clean_topic],
                |row| row.get(0),
            )
            .map_err(|e| format!("Get topic id error: {}", e))?;

        let sources_json = if rec.sources.is_empty() {
            String::new()
        } else {
            serde_json::to_string(rec.sources).unwrap_or_default()
        };

        conn.execute(
            "INSERT INTO messages (topic_id, role, content, image_url, thought, meta, sources_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                topic_id,
                rec.role,
                rec.content,
                rec.image_url,
                rec.thought,
                rec.meta,
                sources_json
            ],
        )
        .map_err(|e| format!("Insert message error: {}", e))?;

        Ok(())
    }

    pub fn get_messages(&self, topic_name: &str) -> Result<Vec<ChatMessage>, String> {
        let conn = self.get_conn()?;
        let topic_id: Result<i64, _> = conn.query_row(
            "SELECT id FROM topics WHERE name = ?1",
            params![topic_name],
            |row| row.get(0),
        );

        let topic_id = match topic_id {
            Ok(id) => id,
            Err(_) => return Ok(Vec::new()),
        };

        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, image_url, created_at, \
                 COALESCE(thought, ''), COALESCE(meta, ''), COALESCE(sources_json, '') \
                 FROM messages WHERE topic_id = ?1 ORDER BY id ASC",
            )
            .map_err(|e| format!("Prepare error: {}", e))?;

        let rows = stmt
            .query_map(params![topic_id], |row| {
                let thought: String = row.get(5)?;
                let meta: String = row.get(6)?;
                let sources_json: String = row.get(7)?;
                let sources = if sources_json.trim().is_empty() {
                    Vec::new()
                } else {
                    serde_json::from_str(&sources_json).unwrap_or_default()
                };
                Ok(ChatMessage {
                    id: row.get(0)?,
                    topic_name: topic_name.to_string(),
                    role: row.get(1)?,
                    content: row.get(2)?,
                    image_url: row.get(3)?,
                    created_at: row.get(4)?,
                    thought,
                    meta,
                    sources,
                })
            })
            .map_err(|e| format!("Query map error: {}", e))?;

        let mut msgs = Vec::new();
        for r in rows.flatten() {
            msgs.push(r);
        }
        Ok(msgs)
    }

    pub fn clear_all_history(&self) -> Result<(), String> {
        let conn = self.get_conn()?;
        conn.execute("DELETE FROM messages", [])
            .map_err(|e| format!("Failed to delete messages: {}", e))?;
        conn.execute("DELETE FROM topics", [])
            .map_err(|e| format!("Failed to delete topics: {}", e))?;
        Ok(())
    }

    pub fn delete_topic(&self, topic_name: &str) -> Result<(), String> {
        let conn = self.get_conn()?;
        let topic_id: Result<i64, _> = conn.query_row(
            "SELECT id FROM topics WHERE name = ?1",
            params![topic_name],
            |row| row.get(0),
        );
        if let Ok(id) = topic_id {
            conn.execute("DELETE FROM messages WHERE topic_id = ?1", params![id])
                .map_err(|e| format!("Failed to delete topic messages: {}", e))?;
            conn.execute("DELETE FROM topics WHERE id = ?1", params![id])
                .map_err(|e| format!("Failed to delete topic: {}", e))?;
        }
        Ok(())
    }
}

fn ensure_message_columns(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(messages)")
        .map_err(|e| format!("PRAGMA table_info failed: {e}"))?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("PRAGMA query failed: {e}"))?
        .filter_map(Result::ok)
        .collect();

    for (name, ddl) in [
        ("thought", "ALTER TABLE messages ADD COLUMN thought TEXT DEFAULT ''"),
        ("meta", "ALTER TABLE messages ADD COLUMN meta TEXT DEFAULT ''"),
        ("sources_json", "ALTER TABLE messages ADD COLUMN sources_json TEXT DEFAULT ''"),
    ] {
        if !cols.iter().any(|c| c == name) {
            conn.execute(ddl, [])
                .map_err(|e| format!("Failed migrating messages.{name}: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_messages_and_roundtrips_sources() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE topics (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE NOT NULL, category TEXT NOT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, topic_id INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, image_url TEXT, created_at DATETIME DEFAULT CURRENT_TIMESTAMP);",
        )
        .unwrap();
        ensure_message_columns(&conn).unwrap();
        ensure_message_columns(&conn).unwrap();

        conn.execute("INSERT INTO topics (name, category) VALUES ('Test', 'General')", [])
            .unwrap();
        let src = vec![StoredSearchSource {
            title: "Rust 1.100".into(),
            url: "https://blog.rust-lang.org".into(),
            snippet: "Release notes".into(),
            site_name: "Rust Blog".into(),
            date: "2026/11/10".into(),
        }];
        let json = serde_json::to_string(&src).unwrap();
        conn.execute(
            "INSERT INTO messages (topic_id, role, content, thought, meta, sources_json) VALUES (1, 'assistant', 'Hi [1]', 'Step 1', '1.2 dtk', ?1)",
            params![json],
        )
        .unwrap();
        let (thought, meta, raw_src): (String, String, String) = conn
            .query_row(
                "SELECT COALESCE(thought, ''), COALESCE(meta, ''), COALESCE(sources_json, '') FROM messages WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let decoded: Vec<StoredSearchSource> = serde_json::from_str(&raw_src).unwrap();
        assert_eq!(thought, "Step 1");
        assert_eq!(meta, "1.2 dtk");
        assert_eq!(decoded, src);
    }

    #[test]
    fn resolve_data_dir_distinguishes_portable_vs_cargo_target_and_env() {
        let custom = resolve_data_dir_for_exe(
            Some(Path::new(r"D:\Portable\MiniAssistant")),
            Some(r"C:\CustomData"),
        );
        assert_eq!(custom, PathBuf::from(r"C:\CustomData"));

        let portable = resolve_data_dir_for_exe(
            Some(Path::new(r"D:\Portable\MiniAssistant")),
            None,
        );
        assert_eq!(portable, PathBuf::from(r"D:\Portable\MiniAssistant\data"));

        assert!(is_cargo_target_dir(Path::new(r"C:\repo\ui-native\target\release")));
        assert!(is_cargo_target_dir(Path::new(r"C:\repo\ui-native\target\debug\deps")));
        assert!(!is_cargo_target_dir(Path::new(r"D:\Apps\Mini-Assistant-Portable")));

        let dev_dir = resolve_data_dir_for_exe(
            Some(Path::new(r"C:\repo\ui-native\target\debug")),
            None,
        );
        assert_eq!(dev_dir.file_name().and_then(|s| s.to_str()), Some("MiniAssistant"));
    }
}
