//! The map of registered apps, and the shutdown gate.
//!
//! The API is synchronous and every lock is held for a lookup or an insert only, never across
//! an await. `close` sets the gate and returns every handle in one step, so a start for a new
//! name is either registered before the sweep (and then learns about the shutdown) or refused.

use super::handle::{AppId, ProcessHandle};
use oxidepm_core::{Error, Result};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Default)]
struct Inner {
    by_id: HashMap<AppId, ProcessHandle>,
    by_name: HashMap<String, AppId>,
    closed: bool,
}

#[derive(Default)]
pub struct Registry {
    inner: RwLock<Inner>,
    /// Names whose start is in flight: reserved before the row is written, so two starts of
    /// one new name cannot both create it.
    reserved: Mutex<HashSet<String>>,
}

/// A reserved name. Dropping it releases the reservation.
pub struct NameGuard {
    registry: Arc<Registry>,
    name: String,
}

impl Drop for NameGuard {
    fn drop(&mut self) {
        self.registry.reserved.lock().remove(&self.name);
    }
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn get(&self, id: AppId) -> Option<ProcessHandle> {
        self.inner.read().by_id.get(&id).cloned()
    }

    pub fn by_name(&self, name: &str) -> Option<ProcessHandle> {
        let inner = self.inner.read();
        inner
            .by_name
            .get(name)
            .and_then(|id| inner.by_id.get(id))
            .cloned()
    }

    /// Every handle, sorted by id.
    pub fn list(&self) -> Vec<ProcessHandle> {
        let mut handles: Vec<ProcessHandle> = self.inner.read().by_id.values().cloned().collect();
        handles.sort_by_key(|h| h.id);
        handles
    }

    pub fn is_closed(&self) -> bool {
        self.inner.read().closed
    }

    /// Reserve `name` for a start in flight. `ShuttingDown` once closed, `AppAlreadyExists` if
    /// the name is registered or reserved.
    pub fn reserve_name(self: &Arc<Self>, name: &str) -> Result<NameGuard> {
        let inner = self.inner.read();
        if inner.closed {
            return Err(Error::ShuttingDown);
        }
        if inner.by_name.contains_key(name) {
            return Err(Error::AppAlreadyExists(name.to_string()));
        }
        if !self.reserved.lock().insert(name.to_string()) {
            return Err(Error::AppAlreadyExists(name.to_string()));
        }
        Ok(NameGuard {
            registry: Arc::clone(self),
            name: name.to_string(),
        })
    }

    /// Register a handle under `name`. Re-checks the gate under the same write lock `close`
    /// takes.
    pub fn insert(
        &self,
        name: &str,
        handle: ProcessHandle,
        _guard: Option<NameGuard>,
    ) -> Result<()> {
        let mut inner = self.inner.write();
        if inner.closed {
            return Err(Error::ShuttingDown);
        }
        if inner.by_id.contains_key(&handle.id) || inner.by_name.contains_key(name) {
            return Err(Error::AppAlreadyExists(name.to_string()));
        }
        inner.by_name.insert(name.to_string(), handle.id);
        inner.by_id.insert(handle.id, handle);
        Ok(())
    }

    /// Put a fresh handle in the slot of an app whose task died. Works on a closed registry (the
    /// slot already exists).
    pub fn replace(&self, handle: ProcessHandle) {
        let mut inner = self.inner.write();
        if inner.by_id.contains_key(&handle.id) {
            inner.by_id.insert(handle.id, handle);
        }
    }

    pub fn remove(&self, id: AppId) {
        let mut inner = self.inner.write();
        if inner.by_id.remove(&id).is_some() {
            inner.by_name.retain(|_, v| *v != id);
        }
    }

    /// Close the gate and return every registered handle, atomically. Each handle is marked
    /// closed too, so a request that has not been sent yet is refused.
    pub fn close(&self) -> Vec<ProcessHandle> {
        let mut inner = self.inner.write();
        inner.closed = true;
        let mut handles: Vec<ProcessHandle> = inner.by_id.values().cloned().collect();
        handles.sort_by_key(|h| h.id);
        for handle in &handles {
            handle.close();
        }
        handles
    }
}

#[cfg(test)]
mod tests {
    use super::super::handle::{HandleKind, Snapshot};
    use super::*;
    use oxidepm_core::{AppSpec, Desired, Runtime};

    fn handle(id: AppId, name: &str) -> ProcessHandle {
        let mut spec = AppSpec::new(
            name.to_string(),
            Runtime::CMD,
            "true".to_string(),
            "/".into(),
        );
        spec.id = id;
        let snap = Snapshot::new(Arc::new(spec), Desired::Running, None);
        ProcessHandle::channel(id, HandleKind::Process, snap).0
    }

    #[test]
    fn reserve_insert_and_lookup() {
        let registry = Registry::new();
        let guard = registry.reserve_name("web").unwrap();
        assert!(matches!(
            registry.reserve_name("web"),
            Err(Error::AppAlreadyExists(_))
        ));
        registry
            .insert("web", handle(1, "web"), Some(guard))
            .unwrap();
        // the reservation went with the guard; the registration keeps the name taken
        assert!(matches!(
            registry.reserve_name("web"),
            Err(Error::AppAlreadyExists(_))
        ));
        assert_eq!(registry.by_name("web").unwrap().id, 1);
        assert_eq!(registry.get(1).unwrap().id, 1);
        registry.remove(1);
        assert!(registry.by_name("web").is_none());
        registry.reserve_name("web").unwrap();
    }

    #[test]
    fn dropping_the_guard_releases_the_name() {
        let registry = Registry::new();
        drop(registry.reserve_name("a").unwrap());
        registry.reserve_name("a").unwrap();
    }

    #[test]
    fn close_returns_every_handle_and_refuses_new_names() {
        let registry = Registry::new();
        registry.insert("a", handle(2, "a"), None).unwrap();
        registry.insert("b", handle(1, "b"), None).unwrap();

        let handles = registry.close();
        assert_eq!(handles.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2]);
        assert!(handles.iter().all(|h| h.is_closed()));
        assert!(matches!(
            registry.reserve_name("c"),
            Err(Error::ShuttingDown)
        ));
        assert!(matches!(
            registry.insert("c", handle(3, "c"), None),
            Err(Error::ShuttingDown)
        ));
        assert!(registry.is_closed());
    }

    #[test]
    fn list_is_sorted_by_id() {
        let registry = Registry::new();
        for (id, name) in [(5, "e"), (2, "b"), (9, "i")] {
            registry.insert(name, handle(id, name), None).unwrap();
        }
        assert_eq!(
            registry.list().iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![2, 5, 9]
        );
    }
}
