//! `--ui-gallery`: a debug-only offline driver that shows every `Card`/
//! `Palette` surface state from fixture data, with no network request and no
//! production window classes, hotkeys, tray icon or single-instance mutex
//! touched (issue #363).
//!
//! Split into pure logic (arg parsing, the state list, filenames -- unit
//! tested below, no `windows` crate dependency) and a Win32 driver
//! (`run_gallery`, `#[cfg(debug_assertions)]` only, exercised by the
//! wired-to-nothing manual check described in the PR, not by `cargo test`).

use std::path::PathBuf;

/// Parsed `--ui-gallery [--screenshot <dir>]` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryArgs {
    pub screenshot_dir: Option<PathBuf>,
}

/// Recognizes `--ui-gallery` (optionally followed anywhere later by
/// `--screenshot <dir>`) among `args` (normally `std::env::args().skip(1)`
/// collected by the caller). Returns `None` when `--ui-gallery` is absent.
/// `--screenshot` with no following value is treated as if `--screenshot`
/// were absent (documented, not an error): the gallery still runs
/// interactively rather than aborting on a typo.
pub fn parse_gallery_args(args: &[String]) -> Option<GalleryArgs> {
    if !args.iter().any(|a| a == "--ui-gallery") {
        return None;
    }
    let mut screenshot_dir = None;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--screenshot" {
            if let Some(dir) = iter.next() {
                screenshot_dir = Some(PathBuf::from(dir));
            }
        }
    }
    Some(GalleryArgs { screenshot_dir })
}

/// One state the gallery can drive `Card` or `Palette` into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GalleryState {
    CardAnswerShort,
    CardAnswerDifficultyLow,
    CardAnswerDifficultyMid,
    CardAnswerDifficultyHigh,
    CardAnswerDifficultyUltra,
    CardError,
    CardPending,
    CardPreviewCalendar,
    CardPreviewFormLongLabels,
    PaletteDefault,
    PaletteRouterSuggestion,
    PaletteNoMatches,
    PaletteNoModel,
}

/// The full ordered list of states the gallery steps through. Order is the
/// Right/Left stepping order and the order states are screenshotted in.
pub fn gallery_states() -> Vec<GalleryState> {
    use GalleryState::*;
    vec![
        CardAnswerShort,
        CardAnswerDifficultyLow,
        CardAnswerDifficultyMid,
        CardAnswerDifficultyHigh,
        CardAnswerDifficultyUltra,
        CardError,
        CardPending,
        CardPreviewCalendar,
        CardPreviewFormLongLabels,
        PaletteDefault,
        PaletteRouterSuggestion,
        PaletteNoMatches,
        PaletteNoModel,
    ]
}

/// A stable, filesystem-safe, human-readable name for a state -- used both
/// for the screenshot filename and for any future debug printing. Lowercase,
/// hyphen-separated, no spaces or characters Windows forbids in filenames.
pub fn state_name(state: GalleryState) -> &'static str {
    use GalleryState::*;
    match state {
        CardAnswerShort => "card-answer-short",
        CardAnswerDifficultyLow => "card-answer-difficulty-low",
        CardAnswerDifficultyMid => "card-answer-difficulty-mid",
        CardAnswerDifficultyHigh => "card-answer-difficulty-high",
        CardAnswerDifficultyUltra => "card-answer-difficulty-ultra",
        CardError => "card-error",
        CardPending => "card-pending",
        CardPreviewCalendar => "card-preview-calendar",
        CardPreviewFormLongLabels => "card-preview-form-long-labels",
        PaletteDefault => "palette-default",
        PaletteRouterSuggestion => "palette-router-suggestion",
        PaletteNoMatches => "palette-no-matches",
        PaletteNoModel => "palette-no-model",
    }
}

/// `<state-name>.png`, the file a `--screenshot <dir>` run writes for a
/// given state.
pub fn screenshot_filename(state_name: &str) -> String {
    format!("{state_name}.png")
}

// ---------------------------------------------------------------------------
// Win32 driver (debug builds only)
// ---------------------------------------------------------------------------
//
// Not covered by `cargo test`: it creates real top-level windows and pumps a
// message loop, which needs an interactive desktop session the way
// `Card`/`Palette`'s own tests deliberately avoid needing (they only ever
// call `ShowWindow(SW_SHOWNOACTIVATE)` headlessly). The manual check is
// named in the PR: `wingman --ui-gallery --screenshot <dir>` must produce
// one non-trivial PNG per state in `gallery_states()`, with no hotkey
// registered, no tray icon created and no `single_instance` mutex touched
// (verified by code inspection: this module never imports `hotkey`, `tray`
// or `single_instance`, and never constructs anything from `provider::`
// that would make a real HTTP call -- every value handed to `show_answer`/
// `show_error`/`show_preview` below is a literal or a fixture
// `serde_json::Value`).
#[cfg(debug_assertions)]
mod driver {
    use super::*;
    use crate::provider::Difficulty;
    use crate::ui::card::Card;
    use crate::ui::palette::Palette;
    use crate::ui::palette_model::PaletteAction;
    use std::time::Duration;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, HBITMAP, HDC,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PW_CLIENTONLY};
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_LEFT, VK_RIGHT};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetClientRect, GetMessageW, PeekMessageW, TranslateMessage, MSG,
        PM_REMOVE, WM_KEYDOWN,
    };

    /// Runs the gallery. With `args.screenshot_dir` set, drives every state
    /// non-interactively, screenshots it, and exits; otherwise shows the
    /// first state and lets Left/Right step through the rest until the
    /// window closes.
    pub fn run_gallery(args: GalleryArgs) -> anyhow::Result<()> {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        let instance = unsafe {
            let h = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            windows::Win32::Foundation::HINSTANCE(h.0)
        };

        let mut card = Card::new_for_test(instance)?;
        let mut palette = Palette::new_for_test(instance)?;

        let states = gallery_states();

        if let Some(dir) = &args.screenshot_dir {
            std::fs::create_dir_all(dir)?;
            for state in &states {
                let hwnd = show_state(&mut card, &mut palette, *state);
                pump_and_wait(Duration::from_millis(120));
                let path = dir.join(screenshot_filename(state_name(*state)));
                print_window_png(hwnd, &path)?;
            }
            std::process::exit(0);
        }

        let mut idx = 0usize;
        let hwnd0 = show_state(&mut card, &mut palette, states[idx]);
        let _ = hwnd0;
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if msg.message == WM_KEYDOWN {
                let vk = msg.wParam.0 as u16;
                if vk == VK_RIGHT.0 && idx + 1 < states.len() {
                    idx += 1;
                    show_state(&mut card, &mut palette, states[idx]);
                } else if vk == VK_LEFT.0 && idx > 0 {
                    idx -= 1;
                    show_state(&mut card, &mut palette, states[idx]);
                }
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    fn pump_and_wait(dur: Duration) {
        let deadline = std::time::Instant::now() + dur;
        let mut msg = MSG::default();
        while std::time::Instant::now() < deadline {
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn calendar_fixture() -> (serde_json::Value, serde_json::Value) {
        let schema = crate::actions::schema::schema_for("calendar_event", false)
            .expect("calendar_event is a registered action schema");
        let value = serde_json::json!({
            "title": "Standup",
            "start": "09:00",
            "end": "09:15",
            "location": "Room 2",
            "notes": "bring laptop"
        });
        (schema, value)
    }

    fn form_long_labels_fixture() -> (serde_json::Value, serde_json::Value) {
        let schema = crate::actions::schema::schema_for("form_fill", false)
            .expect("form_fill is a registered action schema");
        let value = serde_json::json!({
            "fields": [
                {
                    "control_id": "ctl1",
                    "source": "profile",
                    "profile_field": "Employer-provided legal first and middle name",
                    "value": "Jordan Alexander",
                    "sensitive": false
                },
                {
                    "control_id": "ctl2",
                    "source": "profile",
                    "profile_field": "Mailing address, including apartment or unit number",
                    "value": "742 Evergreen Terrace, Apt 4B",
                    "sensitive": false
                },
                {
                    "control_id": "ctl3",
                    "source": "profile",
                    "profile_field": "Social security number (last four digits only)",
                    "value": "6789",
                    "sensitive": true
                }
            ]
        });
        (schema, value)
    }

    fn free_palette_catalogue() -> Vec<PaletteAction> {
        vec![
            PaletteAction {
                id: "check-work".to_string(),
                name: "Check my work".to_string(),
                group: Some("Answer".to_string()),
                requires_model: true,
            },
            PaletteAction {
                id: "summarize".to_string(),
                name: "Summarize this".to_string(),
                group: Some("Answer".to_string()),
                requires_model: true,
            },
            PaletteAction {
                id: crate::ui::palette_model::CALCULATE_SELECTION_ACTION_ID.to_string(),
                name: "Calculate selection".to_string(),
                group: Some("Utilities".to_string()),
                requires_model: false,
            },
            PaletteAction {
                id: crate::ui::palette_model::COPY_REGION_ACTION_ID.to_string(),
                name: "Copy region to clipboard".to_string(),
                group: Some("Utilities".to_string()),
                requires_model: false,
            },
        ]
    }

    fn show_state(card: &mut Card, palette: &mut Palette, state: GalleryState) -> HWND {
        use GalleryState::*;
        match state {
            CardAnswerShort => {
                card.hide();
                palette.hide();
                card.show_answer("2 + 2 = 4", "You carried correctly.", 0, None);
            }
            CardAnswerDifficultyLow => {
                card.hide();
                palette.hide();
                card.show_answer(
                    "2 + 2 = 4",
                    "You carried correctly.",
                    0,
                    Some(Difficulty::Level(1)),
                );
            }
            CardAnswerDifficultyMid => {
                card.hide();
                palette.hide();
                card.show_answer(
                    "The limit is 1/2",
                    "Apply L'Hopital's rule once and simplify.",
                    0,
                    Some(Difficulty::Level(5)),
                );
            }
            CardAnswerDifficultyHigh => {
                card.hide();
                palette.hide();
                card.show_answer(
                    "The eigenvalues are 3 and -1",
                    "Solve det(A - lambda I) = 0 for the 2x2 matrix shown.",
                    0,
                    Some(Difficulty::Level(10)),
                );
            }
            CardAnswerDifficultyUltra => {
                card.hide();
                palette.hide();
                card.show_answer(
                    "This needs a specialist",
                    "The screenshot shows a graduate-level proof; a general model should not guess here.",
                    0,
                    Some(Difficulty::Ultra),
                );
            }
            CardError => {
                card.hide();
                palette.hide();
                card.show_error(
                    "Couldn't reach the model",
                    "The request timed out. Check your connection and try again.",
                );
            }
            CardPending => {
                card.hide();
                palette.hide();
                card.show_pending();
            }
            CardPreviewCalendar => {
                card.hide();
                palette.hide();
                let (schema, value) = calendar_fixture();
                let _ = card.show_preview("Add to calendar", &schema, &value, false);
            }
            CardPreviewFormLongLabels => {
                card.hide();
                palette.hide();
                let (schema, value) = form_long_labels_fixture();
                let _ = card.show_preview("Fill out this form", &schema, &value, false);
            }
            PaletteDefault => {
                card.hide();
                palette.hide();
                palette.show(free_palette_catalogue(), true, "mode: Auto".to_string());
            }
            PaletteRouterSuggestion => {
                card.hide();
                palette.hide();
                palette.show(free_palette_catalogue(), true, "mode: Auto".to_string());
                let generation = palette.router_generation();
                let result = crate::router::RouterResult {
                    summary: "Check my work".to_string(),
                    intent: Some("check-work".to_string()),
                    confidence: 0.92,
                };
                palette.apply_router_suggestion(generation, &result, 0.5);
            }
            PaletteNoMatches => {
                card.hide();
                palette.hide();
                palette.show(free_palette_catalogue(), true, "mode: Auto".to_string());
                palette.set_query_for_test("zzzzznomatch");
            }
            PaletteNoModel => {
                card.hide();
                palette.hide();
                palette.show(free_palette_catalogue(), false, "mode: Auto".to_string());
            }
        }
        match state {
            PaletteDefault | PaletteRouterSuggestion | PaletteNoMatches | PaletteNoModel => {
                palette.hwnd()
            }
            _ => card.hwnd(),
        }
    }

    /// Captures `hwnd`'s client area via `PrintWindow` and saves it as a PNG
    /// at `path`. A specific application window, never the monitor (rule:
    /// this must never call anything from `capture.rs`, which grabs the
    /// screen for the real ask flow).
    fn print_window_png(hwnd: HWND, path: &std::path::Path) -> anyhow::Result<()> {
        unsafe {
            let mut rect = windows::Win32::Foundation::RECT::default();
            GetClientRect(hwnd, &mut rect)?;
            let width = (rect.right - rect.left).max(1);
            let height = (rect.bottom - rect.top).max(1);

            let screen_dc: HDC = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            let bitmap: HBITMAP = CreateCompatibleBitmap(screen_dc, width, height);
            let old = SelectObject(mem_dc, bitmap.into());

            let ok = PrintWindow(hwnd, mem_dc, PW_CLIENTONLY);

            let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
            let mut bmi = windows::Win32::Graphics::Gdi::BITMAPINFO {
                bmiHeader: windows::Win32::Graphics::Gdi::BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>()
                        as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    ..Default::default()
                },
                ..Default::default()
            };
            windows::Win32::Graphics::Gdi::GetDIBits(
                mem_dc,
                bitmap,
                0,
                height as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut bmi,
                windows::Win32::Graphics::Gdi::DIB_RGB_COLORS,
            );

            SelectObject(mem_dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);

            if !ok.as_bool() {
                anyhow::bail!("PrintWindow failed for {path:?}");
            }

            // BGRA -> RGBA for the `image` crate.
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            let img: image::RgbaImage =
                image::ImageBuffer::from_raw(width as u32, height as u32, buf)
                    .ok_or_else(|| anyhow::anyhow!("bad PrintWindow buffer for {path:?}"))?;
            img.save(path)
                .map_err(|e| anyhow::anyhow!("failed to save {path:?}: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(debug_assertions)]
pub use driver::run_gallery;

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_ui_gallery_flag_is_none() {
        assert_eq!(parse_gallery_args(&args(&["--settings"])), None);
        assert_eq!(parse_gallery_args(&args(&[])), None);
    }

    #[test]
    fn ui_gallery_alone() {
        assert_eq!(
            parse_gallery_args(&args(&["--ui-gallery"])),
            Some(GalleryArgs {
                screenshot_dir: None
            })
        );
    }

    #[test]
    fn ui_gallery_with_screenshot_dir() {
        assert_eq!(
            parse_gallery_args(&args(&["--ui-gallery", "--screenshot", "out"])),
            Some(GalleryArgs {
                screenshot_dir: Some(PathBuf::from("out"))
            })
        );
    }

    #[test]
    fn screenshot_with_no_following_arg_is_treated_as_absent() {
        assert_eq!(
            parse_gallery_args(&args(&["--ui-gallery", "--screenshot"])),
            Some(GalleryArgs {
                screenshot_dir: None
            })
        );
    }

    #[test]
    fn garbage_args_interspersed_do_not_confuse_parsing() {
        assert_eq!(
            parse_gallery_args(&args(&[
                "--foo",
                "bar",
                "--ui-gallery",
                "--baz",
                "--screenshot",
                "C:\\out dir",
                "--qux"
            ])),
            Some(GalleryArgs {
                screenshot_dir: Some(PathBuf::from("C:\\out dir"))
            })
        );
    }

    #[test]
    fn screenshot_flag_without_ui_gallery_is_none() {
        // --screenshot on its own (no --ui-gallery) means nothing here; the
        // normal launch path ignores it.
        assert_eq!(parse_gallery_args(&args(&["--screenshot", "out"])), None);
    }

    #[test]
    fn state_list_has_the_expected_count_and_no_duplicates() {
        let states = gallery_states();
        // 5 card-answer variants (short + 4 difficulty badges) + error +
        // pending + 2 previews = 9 card states, + 4 palette states = 13.
        assert_eq!(states.len(), 13);
        let names: std::collections::HashSet<&str> =
            states.iter().map(|s| state_name(*s)).collect();
        assert_eq!(
            names.len(),
            states.len(),
            "every gallery state must have a unique name"
        );
    }

    #[test]
    fn state_names_are_filesystem_safe() {
        for state in gallery_states() {
            let name = state_name(state);
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "state name {name:?} is not lowercase/hyphen/digit only"
            );
            assert!(!name.is_empty());
            assert!(!name.starts_with('-') && !name.ends_with('-'));
        }
    }

    #[test]
    fn screenshot_filenames_are_unique_across_the_state_list() {
        let states = gallery_states();
        let files: std::collections::HashSet<String> = states
            .iter()
            .map(|s| screenshot_filename(state_name(*s)))
            .collect();
        assert_eq!(files.len(), states.len());
        for f in &files {
            assert!(f.ends_with(".png"));
            // Windows-illegal filename characters.
            assert!(!f.contains(|c| "<>:\"/\\|?*".contains(c)));
        }
    }

    #[test]
    fn screenshot_filename_is_deterministic() {
        assert_eq!(screenshot_filename("card-error"), "card-error.png");
        assert_eq!(
            screenshot_filename("card-error"),
            screenshot_filename("card-error")
        );
    }
}
