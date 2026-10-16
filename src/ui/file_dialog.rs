//! Open and save dialogs for an action file (#117). Win32 only: the logic
//! that decides what is allowed lives in `actions::share`. These run a nested
//! message loop, so call them only through `App::run_os_dialog`.
//!
//! Checked by hand: tray > Help and diagnostics > Import action from file
//! opens the dialog filtered to `.toml`; Export action to file opens a save
//! dialog named after the action.

use std::path::PathBuf;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

/// `MAX_PATH` plus room for a long name; the dialog refuses a longer pick.
const PATH_BUF: usize = 1024;

/// "Action files (*.toml)" then "All files", each pair nul-terminated and the
/// list ended by a second nul.
fn filter() -> Vec<u16> {
    "Action files (*.toml)\0*.toml\0All files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect()
}

/// A file name that is safe to suggest: the action id with anything but
/// plain characters turned into `-`.
pub fn suggested_file_name(action_id: &str) -> String {
    let stem: String = action_id
        .chars()
        .take(60)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = if stem.is_empty() {
        "action".to_string()
    } else {
        stem
    };
    format!("{stem}.toml")
}

fn path_from_buffer(buf: &[u16]) -> Option<PathBuf> {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

fn run_dialog(owner: HWND, save: bool, suggested: &str) -> Option<PathBuf> {
    let mut buf = vec![0u16; PATH_BUF];
    for (slot, c) in buf
        .iter_mut()
        .take(PATH_BUF - 1)
        .zip(suggested.encode_utf16())
    {
        *slot = c;
    }
    let filter = filter();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: PATH_BUF as u32,
        lpstrDefExt: w!("toml"),
        Flags: if save {
            OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_OVERWRITEPROMPT | OFN_NOCHANGEDIR
        } else {
            OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR
        },
        ..Default::default()
    };
    // SAFETY: `ofn` points at `buf` and `filter`, which outlive the call.
    let picked = unsafe {
        if save {
            GetSaveFileNameW(&mut ofn)
        } else {
            GetOpenFileNameW(&mut ofn)
        }
    };
    if picked.as_bool() {
        path_from_buffer(&buf)
    } else {
        None
    }
}

/// The file the user chose to import, or `None` if they cancelled.
pub fn pick_action_file(owner: HWND) -> Option<PathBuf> {
    run_dialog(owner, false, "")
}

/// Where the user wants the exported action saved, or `None` if they
/// cancelled.
pub fn pick_save_path(owner: HWND, action_id: &str) -> Option<PathBuf> {
    run_dialog(owner, true, &suggested_file_name(action_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_suggested_name_is_plain() {
        assert_eq!(suggested_file_name("tidy-notes"), "tidy-notes.toml");
        assert_eq!(suggested_file_name("a/b\\c:d"), "a-b-c-d.toml");
        assert_eq!(suggested_file_name(""), "action.toml");
        assert!(suggested_file_name(&"x".repeat(500)).len() < 70);
    }

    #[test]
    fn the_filter_ends_with_a_double_nul() {
        let f = filter();
        assert_eq!(&f[f.len() - 2..], &[0, 0]);
    }

    #[test]
    fn a_buffer_reads_up_to_the_first_nul() {
        let mut buf = vec![0u16; 8];
        assert_eq!(path_from_buffer(&buf), None);
        for (i, c) in "ab".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(path_from_buffer(&buf), Some(PathBuf::from("ab")));
    }
}
