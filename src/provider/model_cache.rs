//! An on-disk copy of each cloud vendor's last good model list (#585).
//!
//! Settings always asks the vendor again; the copy only fills the dropdown
//! first and stays when that refresh fails. The file holds model ids and
//! nothing else: no key, no header, no response body, so there is nothing
//! secret to leak from it.
//!
//! `%LOCALAPPDATA%\Wingman\model-list-<vendor>.json`. Every read and write is
//! best effort: a missing, unreadable, oversized or damaged file reads as "no
//! cache".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::model_list::Vendor;

/// A file claiming more ids or longer ids than any vendor sends is damaged.
const MAX_IDS: usize = 2000;
const MAX_ID_LEN: usize = 200;
/// A file bigger than this is not read: 2000 ids of 200 bytes would fit.
const MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    ids: Vec<String>,
}

fn file_name(vendor: Vendor) -> &'static str {
    match vendor {
        Vendor::OpenAi => "model-list-openai.json",
        Vendor::Anthropic => "model-list-anthropic.json",
        Vendor::Gemini => "model-list-gemini.json",
    }
}

/// The cache file for `vendor`. AGENTS.md rule 9: under `#[cfg(test)]` a
/// per-process folder in `%TEMP%`, never the real path.
fn path_for(vendor: Vendor) -> Option<PathBuf> {
    #[cfg(test)]
    {
        Some(
            std::env::temp_dir()
                .join(format!("wingman-test-model-cache-{}", std::process::id()))
                .join(file_name(vendor)),
        )
    }
    #[cfg(not(test))]
    {
        Some(
            crate::known_folder::local_app_data()
                .ok()?
                .join("Wingman")
                .join(file_name(vendor)),
        )
    }
}

fn ids_ok(ids: &[String]) -> bool {
    !ids.is_empty()
        && ids.len() <= MAX_IDS
        && ids.iter().all(|id| {
            !id.trim().is_empty() && id.len() <= MAX_ID_LEN && !id.chars().any(char::is_control)
        })
}

/// The text to write for `ids`, or `None` when the list is not worth keeping
/// (empty, or an id that is not a model id).
fn encode(ids: &[String]) -> Option<String> {
    if !ids_ok(ids) {
        return None;
    }
    serde_json::to_string(&CacheFile { ids: ids.to_vec() }).ok()
}

/// The list in `text`, or `None` for anything that is not a well formed cache.
fn decode(text: &str) -> Option<Vec<String>> {
    let file: CacheFile = serde_json::from_str(text).ok()?;
    ids_ok(&file.ids).then_some(file.ids)
}

fn read_at(path: &Path) -> Option<Vec<String>> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    decode(&std::fs::read_to_string(path).ok()?)
}

/// Writes through a uniquely named temporary file and a rename, so a crash
/// mid-write leaves the previous list, not half of a new one, and two workers
/// never share a temporary file. A failed write or rename removes the
/// temporary file.
fn write_at(path: &Path, ids: &[String]) -> bool {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let Some(text) = encode(ids) else {
        return false;
    };
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let done = std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_ok();
    if !done {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// The cached list for `vendor`, if there is a usable one. Blocking file read:
/// call it from a worker thread.
pub fn load(vendor: Vendor) -> Option<Vec<String>> {
    read_at(&path_for(vendor)?)
}

/// Keeps `ids` as `vendor`'s list. A list that is empty or has an id that is
/// not a model id leaves the previous file alone. Best effort.
pub fn store(vendor: Vendor, ids: &[String]) {
    if let Some(path) = path_for(vendor) {
        let _ = write_at(&path, ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "wingman-test-model-cache-unit-{}",
                std::process::id()
            ))
            .join(name)
    }

    #[test]
    fn a_list_round_trips_through_the_file() {
        let path = scratch("round-trip.json");
        assert!(write_at(&path, &ids(&["gpt-5", "o3"])));
        assert_eq!(read_at(&path), Some(ids(&["gpt-5", "o3"])));
    }

    #[test]
    fn a_missing_or_damaged_file_is_no_cache() {
        assert_eq!(read_at(&scratch("never-written.json")), None);
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"ids":[]}"#,
            r#"{"ids":[""]}"#,
            r#"{"ids":[1]}"#,
        ] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_empty_or_odd_list_is_never_written_and_keeps_the_old_file() {
        let path = scratch("keeps-old.json");
        assert!(write_at(&path, &ids(&["good"])));
        assert!(!write_at(&path, &[]));
        assert!(!write_at(&path, &ids(&["bad\nid"])));
        assert!(!write_at(&path, &ids(&[&"x".repeat(MAX_ID_LEN + 1)])));
        assert_eq!(read_at(&path), Some(ids(&["good"])));
    }

    #[test]
    fn a_file_over_the_size_cap_is_not_read() {
        let path = scratch("too-big.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let pad = " ".repeat(MAX_FILE_BYTES as usize);
        std::fs::write(&path, format!(r#"{{"ids":["a"]}}{pad}"#)).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        // The target is a directory, so the rename cannot succeed.
        let dir = scratch("target-is-a-dir");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!write_at(&dir, &ids(&["a"])));
        let left: Vec<_> = std::fs::read_dir(dir.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("target-is-a-dir.")
            })
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn the_file_holds_only_ids() {
        let text = encode(&ids(&["gpt-5"])).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["ids"]);
    }

    #[test]
    fn each_vendor_has_its_own_file_and_tests_never_use_the_real_folder() {
        let all = [Vendor::OpenAi, Vendor::Anthropic, Vendor::Gemini];
        let paths: Vec<PathBuf> = all.iter().map(|v| path_for(*v).unwrap()).collect();
        let distinct: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(distinct.len(), 3);
        for p in &paths {
            assert!(p.starts_with(std::env::temp_dir()), "{p:?}");
        }
    }

    #[test]
    fn store_then_load_uses_the_test_path() {
        store(Vendor::Gemini, &ids(&["gemini-3-pro"]));
        assert_eq!(load(Vendor::Gemini), Some(ids(&["gemini-3-pro"])));
    }
}
