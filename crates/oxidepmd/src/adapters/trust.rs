//! Accept signed adapter indexes and retain their sequences and revocations.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use oxidepm_adapter_proto::{verify_index, VerifiedIndex};
use oxidepm_core::{Error, Paths, Result};
use tracing::warn;

use super::host::AdapterHost;
use super::install::{index_error, io_error, now_text, read_capped, write_private_atomic};
use super::store::AdapterStore;

/// The cached index and its signature are read up to these sizes.
const MAX_SIG_BYTES: u64 = 16 * 1024;
const MAX_INDEX_STATE_BYTES: usize = 32 * 1024 * 1024;

fn index_signature(paths: &Paths) -> Option<(String, String)> {
    let bytes = read_capped(
        &paths.adapter_index_file(),
        oxidepm_adapter_proto::INDEX_MAX_BYTES as u64,
    )?;
    let sig = read_capped(&paths.adapter_index_sig(), MAX_SIG_BYTES)?;
    Some((String::from_utf8(bytes).ok()?, String::from_utf8(sig).ok()?))
}

#[derive(Default, serde::Deserialize, serde::Serialize)]
struct IndexState {
    sequences: BTreeMap<String, u64>,
    revocations: BTreeMap<String, String>,
    latest: Option<(String, String)>,
}

impl IndexState {
    fn require_history(&self) -> Result<()> {
        if self.latest.is_none() && !self.sequences.is_empty() {
            return Err(Error::AdapterInvalid("the accepted index is missing or stale; refresh the adapter index to check revocations".into()));
        }
        Ok(())
    }

    fn path(paths: &Paths) -> PathBuf {
        paths.adapters_dir().join("accepted-index.json")
    }

    fn read(paths: &Paths) -> Result<Option<Self>> {
        let path = Self::path(paths);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("reading accepted adapter index state", e)),
            Ok(_) => {}
        }
        let bytes = read_capped(&path, MAX_INDEX_STATE_BYTES as u64).ok_or_else(|| {
            Error::AdapterInvalid("the accepted adapter index state cannot be read".into())
        })?;
        serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            Error::AdapterInvalid(format!("invalid accepted adapter index state: {e}"))
        })
    }

    fn write(&self, paths: &Paths) -> Result<std::io::Result<()>> {
        let bytes = serde_json::to_vec(self).map_err(|e| Error::Internal(e.to_string()))?;
        if bytes.len() > MAX_INDEX_STATE_BYTES {
            return Err(Error::AdapterInvalid(
                "the accepted adapter index state exceeds its size limit".into(),
            ));
        }
        let dir = std::fs::File::open(paths.adapters_dir())
            .map_err(|e| io_error("opening the adapter directory", e))?;
        write_private_atomic(&Self::path(paths), &bytes)
            .map_err(|e| io_error("saving accepted adapter index state", e))?;
        Ok(dir.sync_all())
    }

    fn record(&mut self, verified: &VerifiedIndex) {
        self.sequences
            .insert(verified.signed_by.key_id.clone(), verified.index.sequence);
        for revocation in &verified.index.revoked {
            let reason = if revocation.reason.is_empty() {
                "no reason given"
            } else {
                &revocation.reason
            };
            self.revocations.insert(
                revocation.exe_sha256.to_ascii_lowercase(),
                reason.to_string(),
            );
        }
    }
}

impl AdapterHost {
    // ---- the index -------------------------------------------------------------------------

    fn verify_index_candidate(&self, bytes: &[u8], sig: &str) -> Result<VerifiedIndex> {
        if sig.len() > MAX_SIG_BYTES as usize {
            return Err(Error::AdapterInvalid(
                "the index signature exceeds its size limit".into(),
            ));
        }
        verify_index(bytes, sig, &self.cfg.index_keys).map_err(index_error)
    }

    async fn check_index_candidate(
        &self,
        verified: &VerifiedIndex,
        store: Option<&AdapterStore>,
    ) -> Result<IndexState> {
        let state = self.index_state(store).await?;
        let last = state.sequences.get(&verified.signed_by.key_id).copied();
        verified
            .index
            .check_fresh(Utc::now(), last)
            .map_err(index_error)?;
        Ok(state)
    }

    pub(super) async fn plan_index(&self, bytes: &[u8], sig: &str) -> Result<()> {
        let _admin = self.admin.lock().await;
        let verified = self.verify_index_candidate(bytes, sig)?;
        let store = self.store.lock().clone();
        self.check_index_candidate(&verified, store.as_deref())
            .await?;
        if store.is_none() {
            // An unopened database can still carry sequence history, including from a backup.
            match std::fs::symlink_metadata(self.ctx()?.paths.adapters_db()) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_error("checking for the adapter database", e)),
                Ok(_) => return Err(Error::AdapterInvalid(
                    "the adapter database is not open. Run `oxidepm adapter list --available` to check the index against its history before retrying the dry run".into(),
                )),
            }
        }
        Ok(())
    }

    /// Check an index the CLI fetched (the signature with the embedded keys, the trusted comment,
    /// the limits, the expiry, the sequence against the last one seen under that key), remember its
    /// sequence, keep it as the cached index and disable what it revokes. The caller holds `admin`.
    pub(super) async fn accept_index(
        &self,
        bytes: &[u8],
        sig: &str,
    ) -> Result<(VerifiedIndex, Vec<String>)> {
        let verified = self.verify_index_candidate(bytes, sig)?;
        let store = self.store_or_create().await?;
        let paths = &self.ctx()?.paths;
        let mut state = self.check_index_candidate(&verified, Some(&store)).await?;
        let meta_key = format!("last_sequence:{}", verified.signed_by.key_id);
        state.record(&verified);
        state.latest = Some((
            String::from_utf8(bytes.to_vec()).map_err(|e| Error::AdapterInvalid(e.to_string()))?,
            sig.to_string(),
        ));
        let synced = state.write(paths)?;
        let disabled = self.apply_revocations(&state, &store).await;
        synced.map_err(|e| io_error("syncing accepted adapter index state", e))?;
        // Keep the sequence available to existing backup readers.
        if let Err(e) = store
            .repo()
            .set_meta(&meta_key, &verified.index.sequence.to_string())
            .await
        {
            warn!("Could not mirror the adapter index sequence: {e}");
        }
        if let Err(e) = write_private_atomic(&paths.adapter_index_file(), bytes)
            .and_then(|_| write_private_atomic(&paths.adapter_index_sig(), sig.as_bytes()))
        {
            warn!(
                "Could not keep the index in {}: {}",
                paths.adapter_index().display(),
                e
            );
        }
        Ok((verified, disabled))
    }

    fn legacy_cached_index(&self) -> Option<VerifiedIndex> {
        let paths = &self.ctx.get()?.paths;
        let (bytes, sig) = index_signature(paths)?;
        verify_index(bytes.as_bytes(), &sig, &self.cfg.index_keys).ok()
    }

    pub(super) fn cached_index(&self) -> Option<VerifiedIndex> {
        match IndexState::read(&self.ctx.get()?.paths).ok()? {
            Some(state) => {
                let (bytes, sig) = state.latest?;
                let verified = verify_index(bytes.as_bytes(), &sig, &self.cfg.index_keys).ok()?;
                (state.sequences.get(&verified.signed_by.key_id) == Some(&verified.index.sequence))
                    .then_some(verified)
            }
            None => self.legacy_cached_index(),
        }
    }

    async fn index_state(&self, store: Option<&AdapterStore>) -> Result<IndexState> {
        let paths = &self.ctx()?.paths;
        let mut state = IndexState::read(paths)?.unwrap_or_default();
        let migrate = state.latest.is_none();
        if let Some(store) = store {
            for (key, value) in store.repo().meta_with_prefix("last_sequence:").await? {
                if let (Some(key), Ok(sequence)) =
                    (key.strip_prefix("last_sequence:"), value.parse::<u64>())
                {
                    let high = state.sequences.entry(key.to_string()).or_default();
                    *high = (*high).max(sequence);
                }
            }
        }
        if migrate {
            let keys = &self.cfg.index_keys;
            if let Some((bytes, sig)) = index_signature(paths) {
                if let Ok(verified) = verify_index(bytes.as_bytes(), &sig, keys) {
                    let high = state
                        .sequences
                        .get(&verified.signed_by.key_id)
                        .copied()
                        .unwrap_or(0);
                    if verified.index.sequence >= high {
                        state.record(&verified);
                        state.latest = Some((bytes, sig));
                    }
                }
            }
        }
        Ok(state)
    }

    pub(super) async fn check_revocation(
        &self,
        store: &AdapterStore,
        name: &str,
        hash: &str,
    ) -> Result<()> {
        let state = self.index_state(Some(store)).await?;
        state.require_history()?;
        if let Some(reason) = state.revocations.get(&hash.to_ascii_lowercase()) {
            return Err(Error::AdapterInvalid(format!(
                "the executable of '{name}' is revoked: {reason}"
            )));
        }
        Ok(())
    }

    pub(super) async fn apply_cached_revocations(&self, store: &Arc<AdapterStore>) -> Result<()> {
        let state = self.index_state(Some(store)).await?;
        state.require_history()?;
        self.apply_revocations(&state, store).await;
        Ok(())
    }

    /// Disable every installed adapter whose executable is revoked, write an audit line and
    /// keep the reason for `DaemonStatus.warnings`. Returns the names disabled now.
    async fn apply_revocations(
        &self,
        state: &IndexState,
        store: &Arc<AdapterStore>,
    ) -> Vec<String> {
        let mut disabled = Vec::new();
        let Ok(rows) = store.repo().list().await else {
            return disabled;
        };
        for mut row in rows {
            let Some(reason) = state.revocations.get(&row.exe_sha256.to_ascii_lowercase()) else {
                continue;
            };
            self.revoked.lock().insert(row.name.clone(), reason.clone());
            if !row.enabled {
                continue;
            }
            row.enabled = false;
            row.state_note = Some(format!("revoked: {reason}"));
            row.updated_at = now_text();
            if let Err(e) = store.repo().update(&row).await {
                warn!(
                    "Could not record that adapter '{}' is revoked: {}",
                    row.name, e
                );
            }
            if let Err(e) = self.replace(&row, store).await {
                warn!(
                    "Could not stop adapter '{}' after it was revoked: {}",
                    row.name, e
                );
            }
            self.audit_event(
                &row.name,
                "revoked",
                Some(&row.version),
                Some(&row.exe_sha256),
            );
            disabled.push(row.name.clone());
        }
        disabled
    }
}
