//! Access to `adapters.db`, and the lazy creation of what adapters need on disk.
//!
//! A home with no adapters never has an `adapters.db` or an `adapters/` directory:
//! [`AdapterStore::open_existing`] only looks, and the files and directories are made by
//! [`AdapterStore::open_or_create`], which the first import or enable calls.

use oxidepm_core::{Paths, Result};
use oxidepm_db::{AdapterRepository, Database};

/// The open adapter database.
pub struct AdapterStore {
    db: Database,
}

impl AdapterStore {
    /// Open `adapters.db` if the home has one; `None` (and nothing is created) if it does not.
    /// This is what the daemon does at boot: reading the table is all `AdapterHost::load` needs.
    pub async fn open_existing(paths: &Paths) -> Result<Option<Self>> {
        match std::fs::symlink_metadata(paths.adapters_db()) {
            Ok(_) => Ok(Some(Self {
                db: Database::open_adapters(&paths.adapters_db()).await?,
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Open `adapters.db`, making it, and the private `adapters/` directories, if they are missing.
    /// The directories come first, so a failure to make them leaves no database behind.
    pub async fn open_or_create(paths: &Paths) -> Result<Self> {
        paths.ensure_adapter_dirs()?;
        Ok(Self {
            db: Database::open_adapters(&paths.adapters_db()).await?,
        })
    }

    /// The repository over the open database.
    pub fn repo(&self) -> AdapterRepository {
        self.db.adapters()
    }

    /// Fold the write-ahead log into the file and close every connection.
    pub async fn close(&self) {
        self.db.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_db::AdapterRow;
    use std::os::unix::fs::PermissionsExt;

    fn row(name: &str) -> AdapterRow {
        AdapterRow {
            name: name.to_string(),
            kind: "tool".to_string(),
            version: "1.0.0".to_string(),
            exe_sha256: "a".repeat(64),
            manifest_json: "{}".to_string(),
            source: "bundled".to_string(),
            trust: "pinned".to_string(),
            enabled: false,
            grant_json: None,
            grant_fingerprint: None,
            enforced_json: None,
            prev_version: None,
            prev_exe_sha256: None,
            prev_manifest_json: None,
            prev_grant_json: None,
            state_note: None,
            installed_at: "2026-11-01T10:00:00Z".to_string(),
            updated_at: "2026-11-01T10:00:00Z".to_string(),
        }
    }

    fn mode_of(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn open_existing_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        paths.ensure_dirs().unwrap();
        assert!(AdapterStore::open_existing(&paths).await.unwrap().is_none());
        assert!(!paths.adapters_db().exists());
        assert!(!paths.adapters_dir().exists());
    }

    #[tokio::test]
    async fn adapter_dirs_are_created_lazily_0700_by_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        paths.ensure_dirs().unwrap();

        let store = AdapterStore::open_or_create(&paths).await.unwrap();
        store.repo().insert(&row("check")).await.unwrap();
        store.close().await;

        assert_eq!(mode_of(&paths.adapters_dir()), 0o700);
        assert_eq!(mode_of(&paths.adapter_staging()), 0o700);
        assert_eq!(mode_of(&paths.adapters_db()), 0o600);

        // The second time the daemon only looks, and finds what was stored.
        let store = AdapterStore::open_existing(&paths)
            .await
            .unwrap()
            .expect("adapters.db exists now");
        assert_eq!(
            store.repo().get("check").await.unwrap().unwrap().kind,
            "tool"
        );
        store.close().await;
    }

    #[tokio::test]
    async fn a_blocked_adapter_directory_leaves_no_database_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        paths.ensure_dirs().unwrap();
        std::fs::write(paths.adapters_dir(), b"in the way").unwrap();
        assert!(AdapterStore::open_or_create(&paths).await.is_err());
        assert!(!paths.adapters_db().exists());
    }
}
