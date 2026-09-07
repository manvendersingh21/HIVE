//! Open sensitive SQLite databases with private files before any SQL runs.
//!
//! SQLite's Unix VFS copies the database mode when creating WAL/rollback
//! journals and shared memory. Chmod after schema setup is too late: sidecars
//! have already inherited the old mode. Existing sidecars also need repair.

use std::path::Path;

pub(crate) fn open(path: &Path) -> anyhow::Result<rusqlite::Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }

    #[cfg(unix)]
    {
        use std::fs::{OpenOptions, Permissions};
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        // Atomically create new files privately, without changing the process
        // umask (which would race unrelated threads), or truncating existing data.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(Permissions::from_mode(0o600))?;

        // SQLite resolves symlinks before deriving sidecar names.
        let database = std::fs::canonicalize(path)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = database.as_os_str().to_os_string();
            sidecar.push(suffix);
            match std::fs::set_permissions(&sidecar, Permissions::from_mode(0o600)) {
                Ok(()) => {}
                // A checkpoint may have removed the sidecar. Any replacement
                // inherits the now-private database mode.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    Ok(rusqlite::Connection::open(path)?)
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn assert_private(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut file = path.as_os_str().to_os_string();
            file.push(suffix);
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{file:?} was mode {mode:o}");
        }
    }

    #[test]
    fn private_before_sql_and_after_sidecars_are_recreated() {
        let dir = std::env::temp_dir().join(format!("hive-private-{}", uuid::Uuid::new_v4()));
        let path = dir.join("hive.db");
        {
            let conn = open(&path).unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE secret (value TEXT);
                                INSERT INTO secret VALUES ('private transcript');",
            )
            .unwrap();
            assert_private(&path);
        }
        assert!(!path.with_file_name("hive.db-wal").exists());
        {
            let conn = open(&path).unwrap();
            conn.execute_batch("INSERT INTO secret VALUES ('another transcript');")
                .unwrap();
            assert_private(&path);
            assert_eq!(
                conn.query_row("SELECT count(*) FROM secret", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                2
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repairs_existing_sidecars_before_sql_without_losing_data() {
        let dir = std::env::temp_dir().join(format!("hive-private-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hive.db");
        let legacy = rusqlite::Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE secret (value TEXT);
                              INSERT INTO secret VALUES ('retained transcript');",
            )
            .unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let mut file = path.as_os_str().to_os_string();
            file.push(suffix);
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let conn = open(&path).unwrap();
        assert_private(&path);
        let value: String = conn
            .query_row("SELECT value FROM secret", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "retained transcript");
        drop(conn);
        drop(legacy);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
