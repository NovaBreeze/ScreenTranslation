use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use rusqlite::{Connection, params};
use serde::Serialize;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub created_at: i64,
    pub source_text: String,
    pub translated_text: String,
}

pub struct HistoryDb {
    connection: Connection,
}

impl HistoryDb {
    pub fn open() -> Result<Self> {
        let dirs =
            ProjectDirs::from("", "", "ScreenTranslator").context("无法定位 APPDATA 目录")?;
        std::fs::create_dir_all(dirs.config_dir()).context("创建应用数据目录失败")?;
        let mut connection =
            Connection::open(dirs.config_dir().join("history.db")).context("打开历史数据库失败")?;
        Self::migrate(&mut connection)?;
        Ok(Self { connection })
    }

    #[cfg(test)]
    fn from_connection(mut connection: Connection) -> Result<Self> {
        Self::migrate(&mut connection)?;
        Ok(Self { connection })
    }

    fn migrate(connection: &mut Connection) -> Result<()> {
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("历史数据库版本 {version} 高于当前支持的版本 {SCHEMA_VERSION}");
        }

        let transaction = connection.transaction()?;
        if version == 0 {
            transaction.execute_batch(
                "CREATE TABLE IF NOT EXISTS translation_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                source_text TEXT NOT NULL,
                translated_text TEXT NOT NULL
            );
            PRAGMA user_version = 1;",
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn insert(&self, source: &str, translation: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO translation_history(source_text, translated_text) VALUES (?1, ?2)",
            params![source, translation],
        )?;
        Ok(())
    }

    pub fn list(&self, page: u64, page_size: u64) -> Result<Vec<HistoryEntry>> {
        if page == 0 {
            bail!("页码必须从 1 开始");
        }
        if page_size == 0 {
            bail!("每页条数必须大于 0");
        }

        let offset = page
            .checked_sub(1)
            .and_then(|page| page.checked_mul(page_size))
            .context("分页参数过大")?;
        let limit = i64::try_from(page_size).context("每页条数过大")?;
        let offset = i64::try_from(offset).context("分页偏移量过大")?;
        self.query_entries(
            "SELECT id, created_at, source_text, translated_text
             FROM translation_history
             ORDER BY id DESC
             LIMIT ?1 OFFSET ?2",
            params![limit, offset],
        )
    }

    pub fn clear(&self) -> Result<usize> {
        Ok(self
            .connection
            .execute("DELETE FROM translation_history", [])?)
    }

    #[allow(dead_code)]
    pub fn delete(&self, id: i64) -> Result<bool> {
        Ok(self
            .connection
            .execute("DELETE FROM translation_history WHERE id = ?1", params![id])?
            != 0)
    }

    pub fn export_json(&self) -> Result<String> {
        let entries = self.all_entries()?;
        Ok(serde_json::to_string_pretty(&entries)?)
    }

    pub fn export_csv(&self) -> Result<String> {
        let entries = self.all_entries()?;
        let mut csv = String::from("id,created_at,source_text,translated_text\r\n");
        for entry in entries {
            csv.push_str(&entry.id.to_string());
            csv.push(',');
            csv.push_str(&entry.created_at.to_string());
            csv.push(',');
            push_csv_field(&mut csv, &entry.source_text);
            csv.push(',');
            push_csv_field(&mut csv, &entry.translated_text);
            csv.push_str("\r\n");
        }
        Ok(csv)
    }

    fn all_entries(&self) -> Result<Vec<HistoryEntry>> {
        self.query_entries(
            "SELECT id, created_at, source_text, translated_text
             FROM translation_history
             ORDER BY id DESC",
            [],
        )
    }

    fn query_entries<P>(&self, sql: &str, params: P) -> Result<Vec<HistoryEntry>>
    where
        P: rusqlite::Params,
    {
        let mut statement = self.connection.prepare(sql)?;
        let entries = statement
            .query_map(params, |row| {
                Ok(HistoryEntry {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    source_text: row.get(2)?,
                    translated_text: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries)
    }
}

fn push_csv_field(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        if character == '"' {
            output.push('"');
        }
        output.push(character);
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> HistoryDb {
        HistoryDb::from_connection(Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn lists_entries_with_pagination_newest_first() {
        let database = database();
        database.insert("one", "一").unwrap();
        database.insert("two", "二").unwrap();
        database.insert("three", "三").unwrap();

        let first_page = database.list(1, 2).unwrap();
        let second_page = database.list(2, 2).unwrap();

        assert_eq!(
            first_page
                .iter()
                .map(|entry| entry.source_text.as_str())
                .collect::<Vec<_>>(),
            ["three", "two"]
        );
        assert_eq!(second_page.len(), 1);
        assert_eq!(second_page[0].source_text, "one");
        assert!(database.list(0, 2).is_err());
        assert!(database.list(1, 0).is_err());
    }

    #[test]
    fn deletes_and_clears_entries() {
        let database = database();
        database.insert("one", "一").unwrap();
        database.insert("two", "二").unwrap();
        let entries = database.list(1, 10).unwrap();

        assert!(database.delete(entries[0].id).unwrap());
        assert!(!database.delete(entries[0].id).unwrap());
        assert_eq!(database.clear().unwrap(), 1);
        assert!(database.list(1, 10).unwrap().is_empty());
    }

    #[test]
    fn exports_valid_json_and_escaped_csv() {
        let database = database();
        database
            .insert("comma,\"quote\"\nline", "译文\r\n第二行")
            .unwrap();

        let json = database.export_json().unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded[0]["source_text"], "comma,\"quote\"\nline");

        let csv = database.export_csv().unwrap();
        assert!(csv.starts_with("id,created_at,source_text,translated_text\r\n"));
        assert!(csv.contains("\"comma,\"\"quote\"\"\nline\""));
        assert!(csv.contains("\"译文\r\n第二行\""));
    }

    #[test]
    fn migrates_an_unversioned_database_without_data_loss() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE translation_history (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                    source_text TEXT NOT NULL,
                    translated_text TEXT NOT NULL
                );
                INSERT INTO translation_history(source_text, translated_text)
                VALUES ('before', '之前');",
            )
            .unwrap();

        let database = HistoryDb::from_connection(connection).unwrap();
        let entries = database.list(1, 10).unwrap();
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();

        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_text, "before");
    }

    #[test]
    fn rejects_a_newer_database_schema() {
        let connection = Connection::open_in_memory().unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();

        assert!(HistoryDb::from_connection(connection).is_err());
    }
}
