//! The actors' handle to the database: the single owner of `desired_state` and of the order
//! in which intent is persisted (4.3).
//!
//! - `Start`: `update_spec` and `set_desired(running)` before the pipeline starts (a new row is
//!   inserted `stopped` and flipped on the first successful launch);
//! - `Stop`: `set_desired(stopped)` before the child is terminated;
//! - `Delete`: the row goes first, and the actor exits only if that worked.

use super::handle::AppId;
use oxidepm_core::{AppSpec, Desired, Result};
use oxidepm_db::{AppRecord, Database};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppStore {
    db: Arc<Database>,
}

impl AppStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// A new row. `AppAlreadyExists` if the name is taken.
    pub async fn insert(
        &self,
        spec: &AppSpec,
        parent: Option<AppId>,
        desired: Desired,
    ) -> Result<AppId> {
        self.db.apps().insert_with(spec, parent, desired).await
    }

    /// Insert the parent and its stopped instances in one transaction.
    pub async fn insert_group(
        &self,
        parent: &AppSpec,
        children: &[AppSpec],
        desired: Desired,
    ) -> Result<(AppId, Vec<AppId>)> {
        self.db
            .apps()
            .insert_group_with_desired(parent, children, desired)
            .await
    }

    pub async fn update_spec(&self, id: AppId, spec: &AppSpec) -> Result<()> {
        self.db.apps().update_spec(id, spec).await
    }

    /// Set the policy markers of `ids` in one transaction (`None` leaves a marker as it is).
    pub async fn set_markers(
        &self,
        ids: &[AppId],
        protected: Option<bool>,
        ai_control: Option<bool>,
    ) -> Result<usize> {
        self.db.apps().set_markers(ids, protected, ai_control).await
    }

    pub async fn set_desired(&self, id: AppId, desired: Desired) -> Result<()> {
        self.db.apps().set_desired(id, desired).await
    }

    pub async fn delete(&self, id: AppId) -> Result<bool> {
        self.db.apps().delete(id).await
    }

    /// Every stored app, oldest first (unreadable rows as `Broken`).
    pub async fn records(&self) -> Result<Vec<AppRecord>> {
        self.db.apps().records().await
    }

    pub async fn record_by_id(&self, id: AppId) -> Result<Option<AppRecord>> {
        self.db.apps().record_by_id(id).await
    }

    pub async fn record_by_name(&self, name: &str) -> Result<Option<AppRecord>> {
        self.db.apps().record_by_name(name).await
    }
}
