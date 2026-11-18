//! Process-wide registry that lets gate agents in child threads reach the run owned by their lead.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use crate::state::TeamworkRun;
use crate::state::TicketStatus;

/// Shared `/teamwork` state for every thread hosted by one process.
#[derive(Debug, Default)]
pub struct TeamworkService {
    state: Mutex<ServiceState>,
}

#[derive(Debug, Default)]
struct ServiceState {
    runtimes: HashMap<String, Arc<TeamworkRuntime>>,
    /// Child thread id to parent thread id, for every spawned sub-agent thread.
    parents: HashMap<String, String>,
    /// Threads that edited files and therefore cannot act as independent gates.
    editors: HashSet<String>,
}

impl TeamworkService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ServiceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn register_thread(&self, thread_id: &str, parent_thread_id: Option<&str>) {
        if let Some(parent_thread_id) = parent_thread_id {
            self.state()
                .parents
                .insert(thread_id.to_string(), parent_thread_id.to_string());
        }
    }

    pub(crate) fn forget_lead(&self, thread_id: &str) {
        self.state().runtimes.remove(thread_id);
    }

    pub(crate) fn runtime(&self, lead_thread_id: &str) -> Option<Arc<TeamworkRuntime>> {
        self.state().runtimes.get(lead_thread_id).cloned()
    }

    pub(crate) fn runtime_or_insert(
        &self,
        lead_thread_id: &str,
        state_path: Option<PathBuf>,
    ) -> Arc<TeamworkRuntime> {
        Arc::clone(
            self.state()
                .runtimes
                .entry(lead_thread_id.to_string())
                .or_insert_with(|| {
                    Arc::new(TeamworkRuntime::new(lead_thread_id.to_string(), state_path))
                }),
        )
    }

    /// Whether `thread_id` was spawned, directly or transitively, by `ancestor_thread_id`.
    #[cfg(test)]
    pub(crate) fn is_descendant(&self, thread_id: &str, ancestor_thread_id: &str) -> bool {
        is_descendant_in(&self.state().parents, thread_id, ancestor_thread_id)
    }

    pub(crate) fn mark_editor(&self, thread_id: &str) {
        self.state().editors.insert(thread_id.to_string());
    }

    pub(crate) fn is_editor(&self, thread_id: &str) -> bool {
        self.state().editors.contains(thread_id)
    }

    /// The run whose lead is an ancestor of `thread_id` and which holds `token`.
    pub(crate) fn runtime_for_ticket(
        &self,
        thread_id: &str,
        token: &str,
    ) -> Option<Arc<TeamworkRuntime>> {
        self.ancestor_runtimes(thread_id)
            .into_iter()
            .find(|runtime| {
                runtime.with_run(|run| run.as_ref().is_some_and(|run| run.ticket(token).is_some()))
            })
    }

    /// Whether an ancestor run of `thread_id` has gate tickets waiting for a verdict.
    pub(crate) fn has_open_tickets_for(&self, thread_id: &str) -> bool {
        self.ancestor_runtimes(thread_id).iter().any(|runtime| {
            runtime.with_run(|run| {
                run.as_ref().is_some_and(|run| {
                    run.gates
                        .iter()
                        .any(|ticket| ticket.status == TicketStatus::Open)
                })
            })
        })
    }

    fn ancestor_runtimes(&self, thread_id: &str) -> Vec<Arc<TeamworkRuntime>> {
        let state = self.state();
        state
            .runtimes
            .iter()
            .filter(|(lead, _)| is_descendant_in(&state.parents, thread_id, lead))
            .map(|(_, runtime)| Arc::clone(runtime))
            .collect()
    }
}

fn is_descendant_in(
    parents: &HashMap<String, String>,
    thread_id: &str,
    ancestor_thread_id: &str,
) -> bool {
    let mut current = thread_id;
    // Bound the walk so a corrupted parent map cannot loop forever.
    for _ in 0..64 {
        let Some(parent) = parents.get(current) else {
            return false;
        };
        if parent == ancestor_thread_id {
            return true;
        }
        current = parent;
    }
    false
}

/// The run owned by one lead thread, plus where it is persisted.
#[derive(Debug)]
pub(crate) struct TeamworkRuntime {
    lead_thread_id: String,
    run: Mutex<Option<TeamworkRun>>,
    state_path: Option<PathBuf>,
    save_lock: Mutex<()>,
}

impl TeamworkRuntime {
    fn new(lead_thread_id: String, state_path: Option<PathBuf>) -> Self {
        Self {
            lead_thread_id,
            run: Mutex::new(None),
            state_path,
            save_lock: Mutex::new(()),
        }
    }

    pub(crate) fn lead_thread_id(&self) -> &str {
        &self.lead_thread_id
    }

    /// Runs `f` with exclusive access to the run. Never hold this across an await.
    pub(crate) fn with_run<R>(&self, f: impl FnOnce(&mut Option<TeamworkRun>) -> R) -> R {
        let mut run = self.run.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut run)
    }

    pub(crate) fn snapshot(&self) -> Option<TeamworkRun> {
        self.with_run(|run| run.clone())
    }

    /// Writes the latest run state to disk. Concurrent saves are serialized and each one writes
    /// the newest snapshot, so an older state never overwrites a newer one.
    pub(crate) async fn save(self: &Arc<Self>) {
        if self.state_path.is_none() {
            return;
        }
        let runtime = Arc::clone(self);
        if let Err(error) = tokio::task::spawn_blocking(move || runtime.save_blocking()).await {
            tracing::warn!("teamwork state save task failed: {error}");
        }
    }

    fn save_blocking(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(run) = self.snapshot() else {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    "failed to remove teamwork state {}: {error}",
                    path.display()
                );
            }
            return;
        };
        let contents = match serde_json::to_vec_pretty(&run) {
            Ok(contents) => contents,
            Err(error) => {
                tracing::warn!("failed to serialize teamwork state: {error}");
                return;
            }
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::warn!("failed to create teamwork state directory: {error}");
            return;
        }
        let temp_path = path.with_extension("json.tmp");
        if let Err(error) = std::fs::write(&temp_path, contents) {
            tracing::warn!(
                "failed to write teamwork state {}: {error}",
                temp_path.display()
            );
            return;
        }
        if let Err(error) = std::fs::rename(&temp_path, path) {
            tracing::warn!(
                "failed to replace teamwork state {}: {error}",
                path.display()
            );
        }
    }

    /// Restores an unfinished run persisted by an earlier process.
    pub(crate) async fn load(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let contents = match tokio::fs::read(path).await {
            Ok(contents) => contents,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("failed to read teamwork state {}: {error}", path.display());
                }
                return;
            }
        };
        match serde_json::from_slice::<TeamworkRun>(&contents) {
            Ok(restored) if !restored.phase.is_terminal() => {
                self.with_run(|run| {
                    if run.is_none() {
                        *run = Some(restored);
                    }
                });
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    "ignoring unreadable teamwork state {}: {error}",
                    path.display()
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
