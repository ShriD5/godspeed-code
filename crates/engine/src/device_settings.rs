//! Small per-device setting files in the engine data dir (the host-side
//! defaults its timers act on), written atomically.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::EngineError;

/// The stored setting, or its default when missing or unreadable.
pub(crate) fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub(crate) fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), EngineError> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| EngineError::Other(format!("{}: {err}", path.display())))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
