//! The in-place overlay's pure core (spec
//! `docs/superpowers/specs/2026-10-16-overlay-design.md`). No Win32 here:
//! everything is a function over OCR lines and numbers, so it is unit-tested.
//! The window, the hooks and the main-thread decision path are in
//! `crate::ui::overlay`.

pub mod alpha;
pub mod anchor;
pub mod annotation;
pub mod detect;
pub mod handoff;
pub mod layout;
pub mod run;
pub mod scene;
pub mod session;
pub mod state;
pub mod wire;

#[cfg(test)]
mod tests {
    /// Slice 1 carried a dead-code allowance on every module here because
    /// nothing called them. Slice 2 wired them, and none may come back: a marker that
    /// hides an unused function is how a half-wired feature stays quiet.
    #[test]
    fn no_dead_code_allowance_remains_under_overlay() {
        let marker = ["allow(dead", "_code)"].concat();
        let sources = [
            include_str!("alpha.rs"),
            include_str!("anchor.rs"),
            include_str!("annotation.rs"),
            include_str!("detect.rs"),
            include_str!("handoff.rs"),
            include_str!("layout.rs"),
            include_str!("mod.rs"),
            include_str!("run.rs"),
            include_str!("scene.rs"),
            include_str!("session.rs"),
            include_str!("state.rs"),
            include_str!("wire.rs"),
        ];
        assert!(sources.iter().all(|s| !s.is_empty()), "candidate set");
        for (i, src) in sources.iter().enumerate() {
            let text = src.replace('\r', "");
            assert!(!text.contains(&marker), "file {i} carries {marker}");
        }
    }
}
