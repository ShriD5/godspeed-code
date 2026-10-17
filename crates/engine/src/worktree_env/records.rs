//! Per-worktree environment state on this device: reserved ports, the sleep
//! flag and the rendered setup steps. Persisted in
//! `{data_dir}/worktree-environments.json` so port reservations survive
//! restarts; one lock guards allocation so concurrent creations never share
//! a port.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use zeron_proto::WorktreePorts;

use super::ports::{self, PortRange};
use crate::EngineError;

const FILE_NAME: &str = "worktree-environments.json";
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRecord {
    /// Canonical worktree root; the record's key.
    pub path: String,
    /// Canonical main checkout of the repository.
    pub repo_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    /// The project path the environment config is stored under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_path: Option<String>,
    pub slug: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<WorktreePorts>,
    #[serde(default)]
    pub sleeping: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup_steps: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecordsFile {
    version: u32,
    #[serde(default)]
    worktrees: BTreeMap<String, WorktreeRecord>,
}

pub struct WorktreeRecords {
    path: PathBuf,
    port_range: PortRange,
    state: Mutex<RecordsFile>,
}

impl WorktreeRecords {
    pub fn open(data_dir: &Path, port_range: PortRange) -> Self {
        let path = data_dir.join(FILE_NAME);
        let state = std::fs::read(&path)
            .ok()
            .and_then(|bytes| match serde_json::from_slice::<RecordsFile>(&bytes) {
                Ok(file) if file.version == VERSION => Some(file),
                Ok(_) | Err(_) => {
                    tracing::warn!(path = %path.display(), "unreadable worktree environments; starting empty");
                    None
                }
            })
            .unwrap_or(RecordsFile {
                version: VERSION,
                worktrees: BTreeMap::new(),
            });
        Self {
            path,
            port_range,
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> MutexGuard<'_, RecordsFile> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get(&self, path: &Path) -> Option<WorktreeRecord> {
        self.lock().worktrees.get(&key(path)).cloned()
    }

    /// The record of the worktree containing `path` (a worktree root or any
    /// folder inside one).
    pub fn containing(&self, path: &Path) -> Option<WorktreeRecord> {
        let path = canonical(path);
        self.lock()
            .worktrees
            .values()
            .filter(|record| path.starts_with(&record.path))
            .max_by_key(|record| record.path.len())
            .cloned()
    }

    pub fn all(&self) -> Vec<WorktreeRecord> {
        self.lock().worktrees.values().cloned().collect()
    }

    /// Insert `record`, reserving a fresh port block of `block` when given.
    /// Allocation and persistence happen under one lock.
    pub fn insert(
        &self,
        mut record: WorktreeRecord,
        block: Option<&zeron_proto::WorktreePortBlock>,
    ) -> Result<WorktreeRecord, EngineError> {
        let mut state = self.lock();
        if let Some(block) = block {
            let reserved: Vec<(u16, u16)> = state
                .worktrees
                .values()
                .filter(|other| other.path != record.path)
                .filter_map(|other| other.ports.as_ref())
                .map(|ports| (ports.base, ports.last()))
                .collect();
            let base = ports::allocate(self.port_range, block.count, &reserved, ports::is_free)
                .ok_or_else(|| {
                    EngineError::Other(format!(
                        "No free block of {} ports between {} and {}",
                        block.count, self.port_range.start, self.port_range.end
                    ))
                })?;
            record.ports = Some(WorktreePorts {
                base,
                count: block.count,
                names: block.names.clone(),
            });
        }
        let mut next = state.worktrees.clone();
        next.insert(record.path.clone(), record.clone());
        self.persist(&next)?;
        state.worktrees = next;
        Ok(record)
    }

    pub fn update(
        &self,
        path: &Path,
        change: impl FnOnce(&mut WorktreeRecord),
    ) -> Result<Option<WorktreeRecord>, EngineError> {
        let mut state = self.lock();
        let key = key(path);
        let Some(mut record) = state.worktrees.get(&key).cloned() else {
            return Ok(None);
        };
        change(&mut record);
        let mut next = state.worktrees.clone();
        next.insert(key, record.clone());
        self.persist(&next)?;
        state.worktrees = next;
        Ok(Some(record))
    }

    pub fn remove(&self, path: &Path) -> Result<Option<WorktreeRecord>, EngineError> {
        let mut state = self.lock();
        let key = key(path);
        if !state.worktrees.contains_key(&key) {
            return Ok(None);
        }
        let mut next = state.worktrees.clone();
        let removed = next.remove(&key);
        self.persist(&next)?;
        state.worktrees = next;
        Ok(removed)
    }

    fn persist(&self, worktrees: &BTreeMap<String, WorktreeRecord>) -> Result<(), EngineError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| EngineError::Other("worktree environments have no folder".into()))?;
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        let file = RecordsFile {
            version: VERSION,
            worktrees: worktrees.clone(),
        };
        serde_json::to_writer_pretty(&mut temp, &file)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path)
            .map_err(|error| EngineError::Io(error.error))?;
        Ok(())
    }
}

/// Canonical through the deepest existing ancestor, so `/var` and
/// `/private/var` agree and a removed worktree still finds its record.
pub fn canonical(path: &Path) -> PathBuf {
    for base in path.ancestors() {
        if let Ok(resolved) = std::fs::canonicalize(base) {
            return match path.strip_prefix(base) {
                Ok(rest) if !rest.as_os_str().is_empty() => resolved.join(rest),
                _ => resolved,
            };
        }
    }
    path.to_path_buf()
}

pub fn key(path: &Path) -> String {
    canonical(path).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &Path) -> WorktreeRecord {
        WorktreeRecord {
            path: key(path),
            repo_path: "/repo".into(),
            space_id: None,
            space_path: None,
            slug: "swift-otter".into(),
            branch: "godspeed/swift-otter".into(),
            ports: None,
            sleeping: false,
            setup_steps: Vec::new(),
        }
    }

    #[test]
    fn reservations_are_unique_and_survive_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let range = PortRange {
            start: 47000,
            end: 47999,
        };
        let block = zeron_proto::WorktreePortBlock {
            count: 4,
            names: BTreeMap::from([("web".into(), 0)]),
        };
        let records = WorktreeRecords::open(temp.path(), range);
        let mut bases = Vec::new();
        for name in ["a", "b", "c"] {
            let folder = temp.path().join(name);
            std::fs::create_dir(&folder).unwrap();
            let saved = records.insert(record(&folder), Some(&block)).unwrap();
            bases.push(saved.ports.unwrap().base);
        }
        let mut blocks: Vec<(u16, u16)> = bases.iter().map(|base| (*base, base + 3)).collect();
        blocks.sort();
        assert!(blocks.windows(2).all(|pair| pair[0].1 < pair[1].0));

        let reopened = WorktreeRecords::open(temp.path(), range);
        let again = reopened.get(&temp.path().join("b")).unwrap();
        assert_eq!(again.ports.as_ref().unwrap().base, bases[1]);
        assert_eq!(again.ports.unwrap().names["web"], 0);

        reopened.remove(&temp.path().join("a")).unwrap();
        let folder = temp.path().join("d");
        std::fs::create_dir(&folder).unwrap();
        let reused = reopened.insert(record(&folder), Some(&block)).unwrap();
        assert!(!bases[1..].contains(&reused.ports.unwrap().base));
    }

    #[test]
    fn containing_finds_the_worktree_of_a_nested_folder() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("wt");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        let records = WorktreeRecords::open(temp.path(), PortRange::DEFAULT);
        records.insert(record(&worktree), None).unwrap();
        assert!(records.containing(&worktree.join("src")).is_some());
        assert!(records.containing(temp.path()).is_none());
        assert!(records.containing(&temp.path().join("wt-other")).is_none());
    }
}
