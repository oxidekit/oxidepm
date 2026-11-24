//! Plans: what a request would do, for `--dry-run`.
//!
//! Everything here is read-only. A plan is built from the registry's published snapshots, the
//! stored files and the pure checks of the supervisor; it never sends a command to an actor,
//! writes a row or a file, calls an adapter or spawns a process. Each function names the apps it
//! was given (the ids the policy authorized) and looks at nothing else.

use super::handle::HandleKind;
use super::*;
use oxidepm_ipc::PlannedItem;

/// `E_...: message`, the text of a `refuse` item.
pub(super) fn refusal(error: &Error) -> String {
    format!("{}: {}", error.code().as_str(), error)
}

/// A size as the plans print it.
fn bytes(n: u64) -> String {
    format!("{n} B")
}

impl Supervisor {
    /// The registered app behind `id`, or the item that says it is gone (the request was looked
    /// at a moment ago; an app deleted since is nothing to plan).
    fn planned_handle(&self, id: u32) -> std::result::Result<ProcessHandle, PlannedItem> {
        self.registry.get(id).ok_or_else(|| {
            PlannedItem::new(Some(id), id.to_string(), "noop")
                .detail("the app is not registered (it is gone)")
        })
    }

    fn instances_of(&self, handle: &ProcessHandle) -> Vec<ProcessHandle> {
        match handle.kind {
            HandleKind::Group => handle
                .snapshot()
                .children
                .iter()
                .filter_map(|id| self.registry.get(*id))
                .collect(),
            HandleKind::Process => Vec::new(),
        }
    }

    /// The log files of an app that exist: the active files and the rotated ones, with sizes.
    fn log_files(&self, name: &str) -> Vec<(String, u64)> {
        let prefixes = [
            format!("{name}-out.log"),
            format!("{name}-err.log"),
            format!("{name}-hooks.log"),
        ];
        let Ok(entries) = std::fs::read_dir(self.paths.logs_dir()) else {
            return Vec::new();
        };
        let mut files: Vec<(String, u64)> = entries
            .flatten()
            .filter_map(|entry| {
                let file = entry.file_name().to_string_lossy().to_string();
                if !prefixes.iter().any(|p| file.starts_with(p.as_str())) {
                    return None;
                }
                let meta = entry.metadata().ok()?;
                meta.is_file().then_some((file, meta.len()))
            })
            .collect();
        files.sort();
        files
    }

    /// `stop`: the apps and their instances in the order the stop takes them, dependents first
    /// (the layer is the `order`); a cluster's own stop comes after its instances. An app at rest
    /// is a `noop`.
    pub fn plan_stop(&self, ids: &[u32]) -> Vec<PlannedItem> {
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        let mut all: Vec<ProcessHandle> = Vec::new();
        for id in ids {
            if !seen.insert(*id) {
                continue;
            }
            match self.planned_handle(*id) {
                Ok(handle) => {
                    let children = self.instances_of(&handle);
                    all.push(handle);
                    for child in children {
                        if seen.insert(child.id) {
                            all.push(child);
                        }
                    }
                }
                Err(gone) => items.push(gone),
            }
        }
        let layers = Self::stop_layers(&all);
        let after = layers.len() as u32;
        for (index, layer) in layers.iter().enumerate() {
            for handle in layer {
                items.push(stop_item(handle).order(index as u32));
            }
        }
        for handle in all.iter().filter(|h| h.kind == HandleKind::Group) {
            let snap = handle.snapshot();
            let active = snap.state.status.is_active()
                || snap.children.iter().any(|c| {
                    self.registry
                        .get(*c)
                        .is_some_and(|h| h.snapshot().state.status.is_active())
                });
            let mut item = PlannedItem::new(
                Some(handle.id),
                snap.spec.name.clone(),
                if active { "stop" } else { "noop" },
            );
            item = item.detail(format!(
                "cluster of {} instance(s); stopped after its instances",
                snap.children.len()
            ));
            items.push(item.order(after));
        }
        items
    }

    /// `restart` and `reload` (a reload is a restart): an app that is up is restarted without
    /// waiting for its dependencies (the gate is for a start), an app at rest is started and
    /// does wait for them.
    pub fn plan_restart(&self, ids: &[u32], reload: bool) -> Result<Vec<PlannedItem>> {
        self.refuse_if_closed()?;
        let mut items = Vec::new();
        for id in ids {
            let handle = match self.planned_handle(*id) {
                Ok(handle) => handle,
                Err(gone) => {
                    items.push(gone);
                    continue;
                }
            };
            let snap = handle.snapshot();
            let (action, mut detail) = match snap.state.status {
                AppStatus::Stopped | AppStatus::Errored => (
                    "start",
                    format!(
                        "the app is {}: it is started and waits for its dependencies first",
                        snap.state.status
                    ),
                ),
                AppStatus::Waiting | AppStatus::Building => (
                    "restart",
                    format!(
                        "the app is {}: the pending start begins over",
                        snap.state.status
                    ),
                ),
                status => (
                    "restart",
                    format!("dependency gate skipped, the app is up ({status})"),
                ),
            };
            if handle.kind == HandleKind::Group {
                detail.push_str(&format!("; cluster of {} instance(s)", snap.children.len()));
            }
            if reload {
                detail.push_str("; a reload is a restart");
            }
            items.push(
                PlannedItem::new(Some(handle.id), snap.spec.name.clone(), action).detail(detail),
            );
        }
        Ok(items)
    }

    /// `delete`: the apps, in the order they are deleted (the order of the request, an app's
    /// instances with it), and the log files that stay on disk.
    pub fn plan_delete(&self, ids: &[u32]) -> Vec<PlannedItem> {
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        let mut order = 0u32;
        for id in ids {
            if !seen.insert(*id) {
                continue;
            }
            let handle = match self.planned_handle(*id) {
                Ok(handle) => handle,
                Err(gone) => {
                    items.push(gone);
                    continue;
                }
            };
            let snap = handle.snapshot();
            let children = self.instances_of(&handle);
            let mut detail = format!("status {}", snap.state.status);
            if handle.kind == HandleKind::Group {
                detail.push_str(&format!(
                    "; cluster of {} instance(s), deleted with it",
                    children.len()
                ));
            }
            items.push(
                PlannedItem::new(Some(handle.id), snap.spec.name.clone(), "delete")
                    .detail(format!(
                        "{}; {}",
                        detail,
                        self.log_remainder(&snap.spec.name)
                    ))
                    .order(order),
            );
            for child in children {
                if seen.insert(child.id) {
                    let name = child.snapshot().spec.name.clone();
                    items.push(
                        PlannedItem::new(Some(child.id), name.clone(), "delete")
                            .detail(format!(
                                "instance of '{}', status {}; {}",
                                snap.spec.name,
                                child.snapshot().state.status,
                                self.log_remainder(&name)
                            ))
                            .order(order),
                    );
                }
            }
            order += 1;
        }
        items
    }

    fn log_remainder(&self, name: &str) -> String {
        let files = self.log_files(name);
        if files.is_empty() {
            return "no log files".to_string();
        }
        let list: Vec<String> = files
            .iter()
            .map(|(file, size)| format!("{file} ({})", bytes(*size)))
            .collect();
        format!("the log files stay: {}", list.join(", "))
    }

    /// `flush`: the files that would be emptied, with their sizes (a cluster's instances
    /// carry the files).
    pub fn plan_flush(&self, ids: &[u32]) -> Vec<PlannedItem> {
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(*id) {
                continue;
            }
            let handle = match self.planned_handle(*id) {
                Ok(handle) => handle,
                Err(gone) => {
                    items.push(gone);
                    continue;
                }
            };
            let snap = handle.snapshot();
            if handle.kind == HandleKind::Group {
                let children = self.instances_of(&handle);
                items.push(
                    PlannedItem::new(Some(handle.id), snap.spec.name.clone(), "flush").detail(
                        format!("flushes the logs of its {} instance(s)", children.len()),
                    ),
                );
                for child in children {
                    if seen.insert(child.id) {
                        items.push(self.flush_item(&child));
                    }
                }
            } else {
                items.push(self.flush_item(&handle));
            }
        }
        items
    }

    fn flush_item(&self, handle: &ProcessHandle) -> PlannedItem {
        let name = handle.snapshot().spec.name.clone();
        let files: Vec<String> = ["out", "err"]
            .iter()
            .filter_map(|stream| {
                let path = self.paths.log_path(&name, stream);
                let size = std::fs::metadata(&path).ok().filter(|m| m.is_file())?.len();
                let file = path.file_name()?.to_string_lossy().to_string();
                Some(format!("{file} ({})", bytes(size)))
            })
            .collect();
        let detail = if files.is_empty() {
            "no log files to empty".to_string()
        } else {
            format!("empties {}", files.join(", "))
        };
        PlannedItem::new(Some(handle.id), name, "flush").detail(detail)
    }
}

/// The item for one process in a stop plan.
fn stop_item(handle: &ProcessHandle) -> PlannedItem {
    let snap = handle.snapshot();
    let status = snap.state.status;
    if status.is_active() {
        PlannedItem::new(Some(handle.id), snap.spec.name.clone(), "stop")
            .detail(format!("status {status}"))
    } else {
        PlannedItem::new(Some(handle.id), snap.spec.name.clone(), "noop")
            .detail(format!("already {status}"))
    }
}

// ---- start ------------------------------------------------------------------------------------

/// What every start plan says about the build: it is decided when the app starts, by the runtime,
/// and a rehearsal resolves and builds nothing.
const BUILD_UNKNOWN: &str = "the build steps and the launch command are known only when the app starts; nothing was resolved or built";

impl Supervisor {
    /// `start` of one spec: the checks of a real start (admission, the instance count, the
    /// dependencies, the working directory), stopping before a name is reserved, a row written
    /// or an actor made. A spec that fails them is the real error. The plan has one item.
    pub fn plan_start(&self, spec: AppSpec) -> Result<(Vec<PlannedItem>, Vec<String>)> {
        self.refuse_if_closed()?;
        let spec = self.admit(spec)?;
        self.check_instance_count(&spec)?;
        self.check_dependencies(&spec, &HashSet::new())?;
        if let Some(e) = self.cluster_count_conflict(&spec) {
            return Err(e);
        }
        let item = self.start_item(&spec, &HashSet::new());
        Ok((vec![item], vec![BUILD_UNKNOWN.to_string()]))
    }

    /// `start_batch`: every spec on its own item (a spec the batch would refuse is a `refuse`
    /// item, as in the real answer), the graph check (cycles, unknown dependencies, dependents
    /// of a refused app) and the start layers as the `order` of each item.
    pub fn plan_start_batch(&self, specs: Vec<AppSpec>) -> Result<(Vec<PlannedItem>, Vec<String>)> {
        self.refuse_if_closed()?;
        let mut seen = HashSet::new();
        for spec in &specs {
            if !seen.insert(spec.name.as_str()) {
                return Err(Error::InvalidSpec(format!(
                    "app '{}' is defined more than once",
                    spec.name
                )));
            }
        }
        let order: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        let in_set: HashSet<&str> = order.iter().map(String::as_str).collect();

        let mut failed: HashMap<String, Error> = HashMap::new();
        let mut admitted: Vec<AppSpec> = Vec::new();
        for spec in specs {
            let name = spec.name.clone();
            match self
                .admit(spec)
                .and_then(|spec| self.check_instance_count(&spec).map(|()| spec))
            {
                Ok(spec) => admitted.push(spec),
                Err(e) => {
                    failed.insert(name, e);
                }
            }
        }
        let refs: Vec<&AppSpec> = admitted
            .iter()
            .filter(|s| !failed.contains_key(&s.name))
            .collect();
        let failed = deps::reject_set(&self.registry, &refs, &in_set, failed);

        // the start layers of what can start: edges to apps outside the batch are no layers
        let nodes: Vec<Node> = admitted
            .iter()
            .filter(|s| !failed.contains_key(&s.name))
            .map(|s| {
                let inside = s
                    .depends_on
                    .iter()
                    .filter(|d| in_set.contains(d.as_str()))
                    .cloned()
                    .collect();
                (s.name.clone(), inside)
            })
            .collect();
        let layers: HashMap<String, u32> = topo_layers(&nodes)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .flat_map(|(index, layer)| layer.into_iter().map(move |name| (name, index as u32)))
            .collect();

        let mut by_name: HashMap<String, AppSpec> =
            admitted.into_iter().map(|s| (s.name.clone(), s)).collect();
        let mut items = Vec::with_capacity(order.len());
        for name in order.iter().cloned() {
            if let Some(e) = failed.get(&name) {
                items.push(
                    PlannedItem::new(self.registry.by_name(&name).map(|h| h.id), name, "refuse")
                        .detail(refusal(e)),
                );
                continue;
            }
            let Some(spec) = by_name.remove(&name) else {
                continue;
            };
            if let Some(e) = self.cluster_count_conflict(&spec) {
                items.push(
                    PlannedItem::new(self.registry.by_name(&name).map(|h| h.id), name, "refuse")
                        .detail(refusal(&e)),
                );
                continue;
            }
            let mut item = self.start_item(&spec, &in_set);
            if let Some(layer) = layers.get(&name) {
                item = item.order(*layer);
            }
            items.push(item);
        }
        Ok((items, vec![BUILD_UNKNOWN.to_string()]))
    }

    /// A cluster that exists keeps its instance count for good: its group refuses a start that
    /// names another one, up or at rest, before it looks at anything else.
    fn cluster_count_conflict(&self, spec: &AppSpec) -> Option<Error> {
        let handle = self.registry.by_name(&spec.name)?;
        let have = handle.snapshot().spec.instances;
        (handle.kind == HandleKind::Group && spec.instances != have)
            .then(|| Error::InvalidSpec(instance_count_message(&spec.name, have)))
    }

    /// The item for one admitted spec: refused when the app is up already (or its stored spec
    /// is unusable), else created or started, with what the spec asks for.
    fn start_item(&self, spec: &AppSpec, in_set: &HashSet<&str>) -> PlannedItem {
        let existing = self.registry.by_name(&spec.name);
        let id = existing.as_ref().map(|h| h.id);
        if let Some(handle) = &existing {
            // a row whose spec could not be read is registered as a placeholder without a program
            if handle.snapshot().spec.command.is_empty() {
                let e = Error::StateIncompatible(format!(
                    "stored spec of app '{}' is invalid (delete the app and re-create it)",
                    spec.name
                ));
                return PlannedItem::new(id, spec.name.clone(), "refuse").detail(refusal(&e));
            }
            if deps::already_up(&self.registry, &spec.name) {
                let e = Error::AppAlreadyExists(spec.name.clone());
                return PlannedItem::new(id, spec.name.clone(), "refuse").detail(refusal(&e));
            }
        }
        let mut parts = vec![
            format!("runtime {}", spec.mode),
            format!("program {}", spec.command),
        ];
        if spec.instances > 1 && spec.instance_id.is_none() {
            parts.push(format!("cluster of {} instance(s)", spec.instances));
        }
        match (spec.port, spec.port_range) {
            (Some(port), _) => parts.push(format!("port {port}")),
            (None, Some((low, high))) => parts.push(format!("ports {low}-{high}")),
            _ => {}
        }
        if !spec.depends_on.is_empty() {
            let deps: Vec<String> = spec
                .depends_on
                .iter()
                .map(|dep| {
                    if in_set.contains(dep.as_str()) {
                        format!("{dep} (in this batch)")
                    } else {
                        match self.registry.by_name(dep) {
                            Some(handle) => format!("{dep} ({})", handle.snapshot().state.status),
                            None => format!("{dep} (missing)"),
                        }
                    }
                })
                .collect();
            parts.push(format!("depends on {}", deps.join(", ")));
        }
        match &existing {
            Some(handle) => {
                parts.insert(0, format!("the app exists ({}); it is started with this spec, its stored markers are kept", handle.snapshot().state.status));
                PlannedItem::new(id, spec.name.clone(), "would_start").detail(parts.join("; "))
            }
            None => {
                PlannedItem::new(None, spec.name.clone(), "would_create").detail(parts.join("; "))
            }
        }
    }
}

// ---- save, resurrect, kill, set-policy --------------------------------------------------------

/// The names of at most this many apps are listed in a plan's detail.
const NAMES_SHOWN: usize = 20;

fn names_list(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(NAMES_SHOWN).map(String::as_str).collect();
    if names.len() > NAMES_SHOWN {
        format!(
            "{} and {} more",
            shown.join(", "),
            names.len() - NAMES_SHOWN
        )
    } else {
        shown.join(", ")
    }
}

impl Supervisor {
    /// `save`: the file that would be written and the apps in it, or the refusal it would meet
    /// (`E_SAVE_WOULD_EMPTY`) as a `refuse` item. Only the database is read.
    pub async fn plan_save(&self, force: bool) -> Result<(Vec<PlannedItem>, Vec<String>)> {
        let mut names: Vec<String> = Vec::new();
        let mut warnings = Vec::new();
        for record in self.store().records().await? {
            match record {
                AppRecord::Ok {
                    spec,
                    parent_id: None,
                    desired: Desired::Running,
                } => names.push(spec.name),
                AppRecord::Broken {
                    id,
                    name,
                    parent_id: None,
                    desired: Desired::Running,
                    ..
                } => warnings.push(format!(
                    "'{name}' (id {id}) is not saved: its stored spec is invalid"
                )),
                _ => {}
            }
        }
        let path = self.paths.saved();
        let file = path.file_name().map_or_else(
            || "saved.json".to_string(),
            |f| f.to_string_lossy().to_string(),
        );
        let item =
            if names.is_empty() && !force && persist::saved_app_count(&path).is_some_and(|n| n > 0)
            {
                PlannedItem::new(None, file, "refuse").detail(refusal(&persist::would_empty(&path)))
            } else {
                let list = if names.is_empty() {
                    String::new()
                } else {
                    format!(": {}", names_list(&names))
                };
                PlannedItem::new(None, file, "save").detail(format!(
                    "writes {} app(s) to {}{}",
                    names.len(),
                    path.display(),
                    list
                ))
            };
        Ok((vec![item], warnings))
    }

    /// `resurrect`: what the saved file adds to the registry (`would_import`, the file is read
    /// and not imported), the apps that are meant to run and are not up (`would_start`), the
    /// ones the graph check refuses (`refuse`, with the code) and the start layers as the order.
    /// A saved file that cannot be used is the real error.
    pub async fn plan_resurrect(&self) -> Result<(Vec<PlannedItem>, Vec<String>)> {
        self.refuse_if_closed()?;
        let entries = persist::read_saved(&self.paths.saved())?;
        let mut items: Vec<PlannedItem> = Vec::new();
        let mut imports: Vec<AppSpec> = Vec::new();
        let mut import_names: HashSet<String> = HashSet::new();
        for entry in entries {
            match entry {
                SavedEntry::Bad { name, error } => {
                    let name = name.unwrap_or_else(|| "(unreadable entry)".to_string());
                    items.push(
                        PlannedItem::new(None, name, "refuse")
                            .detail(refusal(&Error::StateIncompatible(error))),
                    );
                }
                SavedEntry::Spec(spec) if spec.instance_id.is_some() => {}
                SavedEntry::Spec(spec) => {
                    if self.registry.by_name(&spec.name).is_some()
                        || !import_names.insert(spec.name.clone())
                    {
                        continue;
                    }
                    let name = spec.name.clone();
                    match self.admit(*spec) {
                        Ok(spec) => imports.push(spec),
                        Err(e) => {
                            items.push(PlannedItem::new(None, name, "refuse").detail(refusal(&e)))
                        }
                    }
                }
            }
        }
        let targets: Vec<ProcessHandle> = self
            .registry
            .list()
            .into_iter()
            .filter(|h| {
                let snap = h.snapshot();
                snap.parent_id.is_none()
                    && snap.desired == Desired::Running
                    && !snap.state.status.is_active()
            })
            .collect();

        let mut specs: Vec<Arc<AppSpec>> =
            targets.iter().map(|h| h.snapshot().spec.clone()).collect();
        specs.extend(imports.iter().cloned().map(Arc::new));
        let in_set: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let refs: Vec<&AppSpec> = specs.iter().map(|s| s.as_ref()).collect();
        let refused = deps::reject_set(&self.registry, &refs, &in_set, HashMap::new());

        let nodes: Vec<Node> = specs
            .iter()
            .filter(|s| !refused.contains_key(&s.name))
            .map(|s| {
                let inside = s
                    .depends_on
                    .iter()
                    .filter(|d| in_set.contains(d.as_str()))
                    .cloned()
                    .collect();
                (s.name.clone(), inside)
            })
            .collect();
        let layers: HashMap<String, u32> = topo_layers(&nodes)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .flat_map(|(index, layer)| layer.into_iter().map(move |name| (name, index as u32)))
            .collect();

        for spec in &imports {
            items.push(match refused.get(&spec.name) {
                Some(e) => PlannedItem::new(None, spec.name.clone(), "refuse").detail(refusal(e)),
                None => {
                    let item = PlannedItem::new(None, spec.name.clone(), "would_import").detail(
                        "in the saved file and not in the registry: it is added, then started",
                    );
                    match layers.get(&spec.name) {
                        Some(layer) => item.order(*layer),
                        None => item,
                    }
                }
            });
        }
        for handle in &targets {
            let snap = handle.snapshot();
            items.push(match refused.get(&snap.spec.name) {
                Some(e) => PlannedItem::new(Some(handle.id), snap.spec.name.clone(), "refuse")
                    .detail(refusal(e)),
                None => {
                    let item =
                        PlannedItem::new(Some(handle.id), snap.spec.name.clone(), "would_start")
                            .detail(format!("it is meant to run and is {}", snap.state.status));
                    match layers.get(&snap.spec.name) {
                        Some(layer) => item.order(*layer),
                        None => item,
                    }
                }
            });
        }
        let warnings = if items.is_empty() {
            vec!["nothing to resurrect: every app that is meant to run is up".to_string()]
        } else {
            Vec::new()
        };
        Ok((items, warnings))
    }

    /// `kill`: the apps the shutdown would stop, in the order it stops them (the plan of a
    /// `stop` of every app). The daemon is not asked to do anything.
    pub fn plan_kill(&self) -> (Vec<PlannedItem>, Vec<String>) {
        let ids: Vec<u32> = self
            .registry
            .list()
            .iter()
            .filter(|h| h.snapshot().parent_id.is_none())
            .map(|h| h.id)
            .collect();
        let warnings = vec![
            "the daemon exits once the apps are stopped, and rewrites the saved file from what was running; nothing was stopped".to_string(),
        ];
        (self.plan_stop(&ids), warnings)
    }

    /// `set-policy`: the marker change of each app named (a cluster with its instances).
    pub fn plan_markers(
        &self,
        ids: &[u32],
        protected: Option<bool>,
        ai_control: Option<bool>,
    ) -> Result<Vec<PlannedItem>> {
        if protected.is_none() && ai_control.is_none() {
            return Err(Error::InvalidSpec(
                "set_policy needs protected and/or ai_control".to_string(),
            ));
        }
        self.refuse_if_closed()?;
        let mut items = Vec::new();
        for id in ids {
            let handle = match self.planned_handle(*id) {
                Ok(handle) => handle,
                Err(gone) => {
                    items.push(gone);
                    continue;
                }
            };
            let mut members = vec![handle.clone()];
            members.extend(self.instances_of(&handle));
            let (was_protected, was_open) =
                (handle.markers().protected(), handle.markers().ai_control());
            let (now_protected, now_open) = (
                protected.unwrap_or(was_protected),
                ai_control.unwrap_or(was_open),
            );
            let changed = members.iter().any(|m| {
                let markers = m.markers();
                (markers.protected(), markers.ai_control())
                    != (
                        protected.unwrap_or(markers.protected()),
                        ai_control.unwrap_or(markers.ai_control()),
                    )
            });
            let mut detail = format!(
                "protected {was_protected} -> {now_protected}, ai_control {was_open} -> {now_open}"
            );
            if members.len() > 1 {
                detail.push_str(&format!(
                    "; the cluster's {} instance(s) follow",
                    members.len() - 1
                ));
            }
            let name = handle.snapshot().spec.name.clone();
            items.push(
                PlannedItem::new(
                    Some(handle.id),
                    name,
                    if changed { "set_policy" } else { "noop" },
                )
                .detail(detail),
            );
        }
        Ok(items)
    }
}
