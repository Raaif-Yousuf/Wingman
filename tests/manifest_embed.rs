//! The exe must carry an application manifest that asks for common controls
//! v6 (#807). `build.rs` embeds `assets/app.rc`; if the manifest line or the
//! file goes missing, Settings silently falls back to classic v5 controls.

use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
        .replace('\r', "")
}

#[test]
fn manifest_requests_common_controls_v6() {
    let manifest = read("assets/app.manifest");
    assert!(!manifest.is_empty());
    assert!(manifest.contains("Microsoft.Windows.Common-Controls"));
    assert!(manifest.contains("version=\"6.0.0.0\""));
    assert!(manifest.contains("6595b64144ccf1df"));
}

#[test]
fn manifest_does_not_set_dpi_awareness() {
    // App::run sets per-monitor v2 at startup; declaring it here too would
    // make that call fail and leave two places to keep in step.
    let manifest = read("assets/app.manifest");
    assert!(!manifest.to_ascii_lowercase().contains("dpiaware"));
}

#[test]
fn resource_script_embeds_the_manifest() {
    let rc = read("assets/app.rc");
    let line = rc
        .lines()
        .find(|l| l.contains("app.manifest"))
        .expect("app.rc must reference app.manifest");
    // Resource type 24 (RT_MANIFEST), id 1 (the process manifest).
    assert!(line.trim_start().starts_with("1 24"), "got: {line}");
}

#[test]
fn manifest_requests_as_invoker() {
    let manifest = read("assets/app.manifest");
    assert!(manifest.contains("requestedExecutionLevel level=\"asInvoker\""));
}

/// The built exe, not just the source: the manifest is stored as plain text
/// in the resource section, so it can be found in the file's bytes. If
/// `build.rs` stopped embedding it, Settings would silently go back to
/// classic controls.
#[test]
fn the_built_exe_carries_the_common_controls_manifest() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_wingman")).expect("read the built exe");
    let needle = b"Microsoft.Windows.Common-Controls";
    assert!(
        exe.windows(needle.len()).any(|w| w == needle),
        "the built exe has no common controls manifest"
    );
    let invoker = b"asInvoker";
    assert!(exe.windows(invoker.len()).any(|w| w == invoker));
}
