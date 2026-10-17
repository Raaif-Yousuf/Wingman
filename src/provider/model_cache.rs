//! A 24 hour on-disk copy of each cloud vendor's model list (#585).
//!
//! Settings shows the cached list the moment it opens, so the dropdown is full
//! before the network answers, and a failed refresh keeps the last good list.
//! The file holds model ids and a save time and nothing else: no key, no
//! header, no response body, so there is nothing secret to leak from it.
//!
//! `%LOCALAPPDATA%\Wingman\model-list-<vendor>.json`. Every read and write is
//! best effort: a missing, unreadable or damaged file reads as "no cache".

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::model_list::Vendor;

/// A list younger than this needs no refresh when Settings opens.
pub const FRESH_FOR: Duration = Duration::from_secs(24 * 60 * 60);
/// A file claiming more ids or longer ids than any vendor sends is damaged.
const MAX_IDS: usize = 2000;
const MAX_ID_LEN: usize = 200;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    saved_at: u64,
    ids: Vec<String>,
}

/// A cached list and when it was saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    pub ids: Vec<String>,
    pub saved_at: u64,
}

impl Cached {
    /// Whether the list is younger than [`FRESH_FOR`] at `now` (seconds since
    /// the Unix epoch). A save time in the future (clock moved back) is not
    /// fresh, so the next open refreshes it.
    pub fn is_fresh(&self, now: u64) -> bool {
        now.checked_sub(self.saved_at)
            .is_some_and(|age| age < FRESH_FOR.as_secs())
    }
}

/// Seconds since the Unix epoch, for [`Cached::is_fresh`]. A clock before 1970
/// reads as 0, which makes every list stale: the safe outcome (it refreshes).
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// The text to write for `ids` saved at `now`, or `None` when the list is not
/// worth keeping (empty, or an id that is not a model id).
fn encode(ids: &[String], now: u64) -> Option<String> {
    if !ids_ok(ids) {
        return None;
    }
    serde_json::to_string(&CacheFile {
        saved_at: now,
        ids: ids.to_vec(),
    })
    .ok()
}

/// The list in `text`, or `None` for anything that is not a well formed cache.
fn decode(text: &str) -> Option<Cached> {
    let file: CacheFile = serde_json::from_str(text).ok()?;
    ids_ok(&file.ids).then_some(Cached {
        ids: file.ids,
        saved_at: file.saved_at,
    })
}

fn read_at(path: &Path) -> Option<Cached> {
    decode(&std::fs::read_to_string(path).ok()?)
}

/// Writes through a temporary file and a rename, so a crash mid-write leaves
/// the previous list, not half of a new one.
fn write_at(path: &Path, ids: &[String], now: u64) -> bool {
    let Some(text) = encode(ids, now) else {
        return false;
    };
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_ok()
}

/// The cached list for `vendor`, if there is a usable one. Blocking file read:
/// call it from a worker thread.
pub fn load(vendor: Vendor) -> Option<Cached> {
    read_at(&path_for(vendor)?)
}

/// Keeps `ids` as `vendor`'s list. A list that is empty or has an id that is
/// not a model id leaves the previous file alone. Best effort.
pub fn store(vendor: Vendor, ids: &[String]) {
    if let Some(path) = path_for(vendor) {
        let _ = write_at(&path, ids, now());
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
        assert!(write_at(&path, &ids(&["gpt-5", "o3"]), 1000));
        let got = read_at(&path).expect("cache");
        assert_eq!(got.ids, ids(&["gpt-5", "o3"]));
        assert_eq!(got.saved_at, 1000);
    }

    #[test]
    fn a_missing_or_damaged_file_is_no_cache() {
        assert_eq!(read_at(&scratch("never-written.json")), None);
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"saved_at":1,"ids":[]}"#,
            r#"{"saved_at":1,"ids":[""]}"#,
            r#"{"saved_at":-1,"ids":["a"]}"#,
        ] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_empty_or_odd_list_is_never_written_and_keeps_the_old_file() {
        let path = scratch("keeps-old.json");
        assert!(write_at(&path, &ids(&["good"]), 5));
        assert!(!write_at(&path, &[], 6));
        assert!(!write_at(&path, &ids(&["bad\nid"]), 6));
        assert!(!write_at(&path, &ids(&[&"x".repeat(MAX_ID_LEN + 1)]), 6));
        assert_eq!(read_at(&path).unwrap().ids, ids(&["good"]));
    }

    #[test]
    fn a_list_is_fresh_for_a_day_and_a_future_save_time_is_not_fresh() {
        let c = Cached {
            ids: ids(&["a"]),
            saved_at: 1000,
        };
        assert!(c.is_fresh(1000));
        assert!(c.is_fresh(1000 + FRESH_FOR.as_secs() - 1));
        assert!(!c.is_fresh(1000 + FRESH_FOR.as_secs()));
        assert!(!c.is_fresh(999));
    }

    #[test]
    fn the_file_holds_only_ids_and_a_time() {
        let text = encode(&ids(&["gpt-5"]), 7).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["ids", "saved_at"]);
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
        let got = load(Vendor::Gemini).expect("cache");
        assert_eq!(got.ids, ids(&["gemini-3-pro"]));
        assert!(got.is_fresh(now()));
    }
}
