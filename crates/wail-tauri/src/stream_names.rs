use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::{info, warn};

const STREAM_NAMES_FILENAME: &str = "stream_names.json";

/// Tauri managed state wrapper for persisted stream names.
pub struct StreamNameConfig {
    pub data_dir: PathBuf,
    pub names: Mutex<HashMap<u16, String>>,
}

/// Load stream names from disk. Returns empty map on any failure.
pub fn load(data_dir: &Path) -> HashMap<u16, String> {
    let path = data_dir.join(STREAM_NAMES_FILENAME);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    // JSON keys are strings, so we parse as HashMap<String, String> then convert
    let string_map: HashMap<String, String> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "Failed to parse stream_names.json");
            return HashMap::new();
        }
    };

    let mut names = HashMap::new();
    for (k, v) in string_map {
        if let Ok(idx) = k.parse::<u16>() {
            names.insert(idx, v);
        }
    }
    if !names.is_empty() {
        info!(count = names.len(), "Loaded stream names");
    }
    names
}

/// Persist stream names to disk. Logs on failure, never panics.
pub fn save(data_dir: &Path, names: &HashMap<u16, String>) {
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        warn!(error = %e, "Failed to create data dir for stream names");
        return;
    }
    let string_map: HashMap<String, &String> = names.iter().map(|(k, v)| (k.to_string(), v)).collect();
    let json = match serde_json::to_string_pretty(&string_map) {
        Ok(j) => j,
        Err(e) => {
            warn!(error = %e, "Failed to serialize stream names");
            return;
        }
    };
    let path = data_dir.join(STREAM_NAMES_FILENAME);
    if let Err(e) = std::fs::write(&path, json) {
        warn!(error = %e, "Failed to write stream_names.json");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_empty_dir_returns_empty() {
        let dir = std::env::temp_dir().join(format!("wail-sn-test-{}", uuid::Uuid::new_v4()));
        let names = load(&dir);
        assert!(names.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wail-sn-test-{}", uuid::Uuid::new_v4()));
        let mut names = HashMap::new();
        names.insert(0, "Bass".to_string());
        names.insert(3, "Drums".to_string());

        save(&dir, &names);
        let loaded = load(&dir);

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[&0], "Bass");
        assert_eq!(loaded[&3], "Drums");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_empty_removes_all() {
        let dir = std::env::temp_dir().join(format!("wail-sn-test-{}", uuid::Uuid::new_v4()));
        let mut names = HashMap::new();
        names.insert(0, "Bass".to_string());
        save(&dir, &names);

        save(&dir, &HashMap::new());
        let loaded = load(&dir);
        assert!(loaded.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
