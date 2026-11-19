//! Process-wide registry that lets member threads reach the swarm owned by their coordinator.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use crate::prompts::board_markdown;
use crate::state::Activity;
use crate::state::SwarmRun;

/// Shared `/swarm` state for every thread hosted by one process.
#[derive(Debug, Default)]
pub struct SwarmService {
    state: Mutex<ServiceState>,
}

#[derive(Debug, Default)]
struct ServiceState {
    /// Coordinator thread id to its run.
    runtimes: HashMap<String, Arc<SwarmRuntime>>,
    /// Child thread id to parent thread id, for every spawned sub-agent thread.
    parents: HashMap<String, String>,
}

impl SwarmService {
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

    pub(crate) fn forget_coordinator(&self, thread_id: &str) {
        self.state().runtimes.remove(thread_id);
    }

    pub(crate) fn runtime(&self, coordinator_thread_id: &str) -> Option<Arc<SwarmRuntime>> {
        self.state().runtimes.get(coordinator_thread_id).cloned()
    }

    pub(crate) fn runtime_or_insert(
        &self,
        coordinator_thread_id: &str,
        state_dir: Option<PathBuf>,
    ) -> Arc<SwarmRuntime> {
        Arc::clone(
            self.state()
                .runtimes
                .entry(coordinator_thread_id.to_string())
                .or_insert_with(|| {
                    Arc::new(SwarmRuntime::new(
                        coordinator_thread_id.to_string(),
                        state_dir,
                    ))
                }),
        )
    }

    /// Runs whose coordinator is an ancestor of `thread_id`, nearest first.
    pub(crate) fn ancestor_runtimes(&self, thread_id: &str) -> Vec<Arc<SwarmRuntime>> {
        let state = self.state();
        let mut found = Vec::new();
        let mut current = thread_id.to_string();
        // Bound the walk so a corrupted parent map cannot loop forever.
        for _ in 0..64 {
            let Some(parent) = state.parents.get(&current) else {
                break;
            };
            if let Some(runtime) = state.runtimes.get(parent) {
                found.push(Arc::clone(runtime));
            }
            current = parent.clone();
        }
        found
    }

    /// Whether `thread_id` descends from `ancestor_thread_id`.
    #[cfg(test)]
    pub(crate) fn is_descendant(&self, thread_id: &str, ancestor_thread_id: &str) -> bool {
        let state = self.state();
        let mut current = thread_id;
        for _ in 0..64 {
            let Some(parent) = state.parents.get(current) else {
                return false;
            };
            if parent == ancestor_thread_id {
                return true;
            }
            current = parent;
        }
        false
    }

    /// The run in which `thread_id` is a joined member, with its member name.
    pub(crate) fn membership(&self, thread_id: &str) -> Option<(Arc<SwarmRuntime>, String)> {
        self.ancestor_runtimes(thread_id)
            .into_iter()
            .find_map(|runtime| {
                let name = runtime.with_run(|run| {
                    run.as_ref()
                        .and_then(|run| run.member_by_thread(thread_id))
                        .map(|member| member.name.clone())
                })?;
                Some((runtime, name))
            })
    }

    /// Whether an ancestor run of `thread_id` still has members waiting to join.
    pub(crate) fn has_joinable_run(&self, thread_id: &str) -> bool {
        self.ancestor_runtimes(thread_id).iter().any(|runtime| {
            runtime.with_run(|run| {
                run.as_ref().is_some_and(|run| {
                    run.members
                        .iter()
                        .any(|member| member.activity == Activity::Invited)
                })
            })
        })
    }

    /// The ancestor run that issued `token`.
    pub(crate) fn runtime_for_token(
        &self,
        thread_id: &str,
        token: &str,
    ) -> Option<Arc<SwarmRuntime>> {
        self.ancestor_runtimes(thread_id)
            .into_iter()
            .find(|runtime| {
                runtime.with_run(|run| {
                    run.as_ref().is_some_and(|run| {
                        run.members.iter().any(|member| member.join_token == token)
                    })
                })
            })
    }
}

/// The run owned by one coordinator thread, plus where it is persisted.
#[derive(Debug)]
pub(crate) struct SwarmRuntime {
    coordinator_thread_id: String,
    run: Mutex<Option<SwarmRun>>,
    state_dir: Option<PathBuf>,
    save_lock: Mutex<()>,
}

impl SwarmRuntime {
    fn new(coordinator_thread_id: String, state_dir: Option<PathBuf>) -> Self {
        Self {
            coordinator_thread_id,
            run: Mutex::new(None),
            state_dir,
            save_lock: Mutex::new(()),
        }
    }

    pub(crate) fn coordinator_thread_id(&self) -> &str {
        &self.coordinator_thread_id
    }

    pub(crate) fn state_path(&self) -> Option<PathBuf> {
        self.state_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.json", self.coordinator_thread_id)))
    }

    /// Human-readable board transcript, rewritten on every save so the user can follow it live.
    pub(crate) fn board_path(&self) -> Option<PathBuf> {
        self.state_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.board.md", self.coordinator_thread_id)))
    }

    /// Runs `f` with exclusive access to the run. Never hold this across an await.
    pub(crate) fn with_run<R>(&self, f: impl FnOnce(&mut Option<SwarmRun>) -> R) -> R {
        let mut run = self.run.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut run)
    }

    pub(crate) fn snapshot(&self) -> Option<SwarmRun> {
        self.with_run(|run| run.clone())
    }

    /// Writes the latest run state and board to disk. Concurrent saves are serialized and each
    /// writes the newest snapshot, so an older state never overwrites a newer one.
    pub(crate) async fn save(self: &Arc<Self>) {
        if self.state_dir.is_none() {
            return;
        }
        let runtime = Arc::clone(self);
        if let Err(error) = tokio::task::spawn_blocking(move || runtime.save_blocking()).await {
            tracing::warn!("swarm state save task failed: {error}");
        }
    }

    fn save_blocking(&self) {
        let (Some(path), Some(board_path)) = (self.state_path(), self.board_path()) else {
            return;
        };
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(run) = self.snapshot() else {
            for path in [&path, &board_path] {
                if let Err(error) = std::fs::remove_file(path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!("failed to remove swarm state {}: {error}", path.display());
                }
            }
            return;
        };
        let contents = match serde_json::to_vec_pretty(&run) {
            Ok(contents) => contents,
            Err(error) => {
                tracing::warn!("failed to serialize swarm state: {error}");
                return;
            }
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::warn!("failed to create swarm state directory: {error}");
            return;
        }
        write_atomically(&path, &contents);
        write_atomically(&board_path, board_markdown(&run, None).as_bytes());
    }

    /// Restores an unfinished run persisted by an earlier process.
    pub(crate) async fn load(&self) {
        let Some(path) = self.state_path() else {
            return;
        };
        let contents = match tokio::fs::read(&path).await {
            Ok(contents) => contents,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("failed to read swarm state {}: {error}", path.display());
                }
                return;
            }
        };
        match serde_json::from_slice::<SwarmRun>(&contents) {
            Ok(restored) => {
                self.with_run(|run| {
                    if run.is_none() {
                        *run = Some(restored);
                    }
                });
            }
            Err(error) => {
                tracing::warn!(
                    "ignoring unreadable swarm state {}: {error}",
                    path.display()
                );
            }
        }
    }
}

fn write_atomically(path: &std::path::Path, contents: &[u8]) {
    let mut temp_path = path.as_os_str().to_owned();
    temp_path.push(".tmp");
    let temp_path = PathBuf::from(temp_path);
    if let Err(error) = std::fs::write(&temp_path, contents) {
        tracing::warn!(
            "failed to write swarm state {}: {error}",
            temp_path.display()
        );
        return;
    }
    if let Err(error) = std::fs::rename(&temp_path, path) {
        tracing::warn!("failed to replace swarm state {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn ancestor_runtimes_walk_the_spawn_tree() {
        let service = SwarmService::new();
        service.register_thread("child", Some("coord"));
        service.register_thread("grandchild", Some("child"));
        let runtime = service.runtime_or_insert("coord", None);
        runtime.with_run(|run| {
            *run = Some(SwarmRun::new("r".into(), "coord".into(), "task".into()));
        });
        assert_eq!(service.ancestor_runtimes("grandchild").len(), 1);
        assert!(service.is_descendant("grandchild", "coord"));
        assert!(!service.is_descendant("coord", "child"));
        assert!(service.ancestor_runtimes("stranger").is_empty());
    }

    #[tokio::test]
    async fn save_and_load_round_trip_with_board_transcript() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = SwarmService::new();
        let runtime = service.runtime_or_insert("coord", Some(dir.path().to_path_buf()));
        runtime.with_run(|run| {
            *run = Some(SwarmRun::new("r".into(), "coord".into(), "port it".into()));
        });
        runtime.save().await;
        assert!(dir.path().join("coord.json").exists());
        let board = std::fs::read_to_string(dir.path().join("coord.board.md")).expect("board");
        assert!(board.contains("port it"), "{board}");

        let other = SwarmService::new();
        let restored = other.runtime_or_insert("coord", Some(dir.path().to_path_buf()));
        restored.load().await;
        assert_eq!(
            restored.snapshot().map(|run| run.task),
            Some("port it".into())
        );

        runtime.with_run(|run| *run = None);
        runtime.save().await;
        assert!(!dir.path().join("coord.json").exists());
        assert!(!dir.path().join("coord.board.md").exists());
    }
}
