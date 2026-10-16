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

/// Parsed `--ui-gallery [--screenshot <dir>] [--theme <name>] [--dpi <percent>]
/// [--matrix]` invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GalleryArgs {
    pub screenshot_dir: Option<PathBuf>,
    /// The raw `--theme` value. Checked by the driver with
    /// [`theme_from_arg`], so a typo is an error and not a silent default.
    pub theme: Option<String>,
    /// The raw `--dpi` value, a display scale in percent (`100`, `150`,
    /// `250`). Checked by the driver with [`dpi_from_arg`].
    pub dpi: Option<String>,
    /// `--matrix`: with `--screenshot <dir>`, render every state under every
    /// theme and scale in [`MATRIX_THEMES`] and [`MATRIX_SCALES`].
    pub matrix: bool,
}

/// The themes `--matrix` renders, by their `--theme` names.
#[cfg(any(test, debug_assertions))]
pub const MATRIX_THEMES: [&str; 3] = ["light", "dark", "high-contrast"];

/// The display scales (percent) `--matrix` renders.
#[cfg(any(test, debug_assertions))]
pub const MATRIX_SCALES: [u32; 4] = [100, 150, 200, 250];

/// The DPI a `--dpi` value names. The value is a display scale in percent
/// (`150` or `150%`), 100 to 500, and 100% is 96 DPI. `None` for anything
/// else.
#[cfg(any(test, debug_assertions))]
pub fn dpi_from_arg(value: &str) -> Option<u32> {
    let percent: u32 = value.strip_suffix('%').unwrap_or(value).parse().ok()?;
    (100..=500)
        .contains(&percent)
        .then(|| (percent * 96 + 50) / 100)
}

/// The display scale in percent a DPI is, rounded: 144 is 150.
#[cfg(any(test, debug_assertions))]
pub fn scale_percent(dpi: u32) -> u32 {
    (dpi * 100 + 48) / 96
}

/// The file a `--matrix` run writes: `<state>--<theme>--<percent>.png`, for
/// example `card-error--dark--150.png`. The same names on every run, so two
/// folders can be compared file by file.
#[cfg(any(test, debug_assertions))]
pub fn matrix_filename(state_name: &str, theme_name: &str, percent: u32) -> String {
    format!("{state_name}--{theme_name}--{percent}.png")
}

/// Whether a state can be drawn under a simulated theme and DPI. The card
/// states and the region overlay can. The palette reads the live theme and
/// the window's own DPI, so `--matrix` draws it once, as
/// `<state>--system--<percent>.png` at the DPI the window really has.
/// Settings follows the DPI only (see [`follows_dpi_only`]).
#[cfg(any(test, debug_assertions))]
pub fn follows_variants(state: GalleryState) -> bool {
    use GalleryState::*;
    !matches!(
        state,
        PaletteDefault
            | PaletteRouterSuggestion
            | PaletteNoMatches
            | PaletteNoModel
            | SettingsTop
            | SettingsBottom
            | SettingsAdvancedTop
            | SettingsAdvancedBottom
    )
}

/// Whether a state is captured by sending `WM_PRINT` instead of calling
/// `PrintWindow`. The palette draws with a Direct2D window render target,
/// which `PrintWindow` cannot read, so its pictures were a black page (#626).
/// The card and Settings paint with GDI and `PrintWindow` is exact for them.
#[cfg(any(test, debug_assertions))]
pub fn prints_with_wm_print(state: GalleryState) -> bool {
    use GalleryState::*;
    matches!(
        state,
        PaletteDefault | PaletteRouterSuggestion | PaletteNoMatches | PaletteNoModel
    )
}

/// Whether a state follows `--dpi` but not `--theme`: Settings can be laid out
/// at a simulated scale, but it has no forced theme yet (issue #662), so
/// `--matrix` draws it once per scale as `<state>--system--<percent>.png`.
#[cfg(any(test, debug_assertions))]
pub fn follows_dpi_only(state: GalleryState) -> bool {
    use GalleryState::*;
    matches!(
        state,
        SettingsTop | SettingsBottom | SettingsAdvancedTop | SettingsAdvancedBottom
    )
}

/// The theme a `--theme` value names: `light`, `dark` or `high-contrast`
/// (Windows' high contrast black). `None` for anything else.
#[cfg(any(test, debug_assertions))]
pub fn theme_from_arg(name: &str) -> Option<crate::ui::theme::Theme> {
    use crate::ui::theme::Theme;
    match name {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        "high-contrast" => Some(Theme::HighContrast),
        _ => None,
    }
}

/// Recognizes `--ui-gallery` (optionally followed anywhere later by
/// `--screenshot <dir>`, `--theme <name>`, `--dpi <percent>` and `--matrix`) among `args` (normally `std::env::args().skip(1)`
/// collected by the caller). Returns `None` when `--ui-gallery` is absent.
/// `--screenshot` with no following value is treated as if `--screenshot`
/// were absent (documented, not an error): the gallery still runs
/// interactively rather than aborting on a typo.
pub fn parse_gallery_args(args: &[String]) -> Option<GalleryArgs> {
    if !args.iter().any(|a| a == "--ui-gallery") {
        return None;
    }
    let mut screenshot_dir = None;
    let mut theme = None;
    let mut dpi = None;
    let mut matrix = false;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--screenshot" {
            if let Some(dir) = iter.next() {
                screenshot_dir = Some(PathBuf::from(dir));
            }
        } else if a == "--theme" {
            if let Some(name) = iter.next() {
                theme = Some(name.clone());
            }
        } else if a == "--dpi" {
            if let Some(value) = iter.next() {
                dpi = Some(value.clone());
            }
        } else if a == "--matrix" {
            matrix = true;
        }
    }
    Some(GalleryArgs {
        screenshot_dir,
        theme,
        dpi,
        matrix,
    })
}

/// One state the gallery can drive `Card` or `Palette` into.
#[cfg(any(debug_assertions, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GalleryState {
    CardAnswerShort,
    CardAnswerDifficultyLow,
    CardAnswerDifficultyMid,
    CardAnswerDifficultyHigh,
    CardAnswerDifficultyUltra,
    /// A model answer opened up, so both footer links (Read aloud, Copy) show.
    CardAnswerExpanded,
    /// The same with the Tab focus rectangle on the first link (#606).
    CardAnswerFocused,
    CardError,
    /// #831: the first-launch welcome card (a click-to-open-Settings card).
    CardWelcome,
    /// Issue #354. Replaces the old single glyph-only `CardPending` state:
    /// the pending card now always carries a status line, so each sub-stage
    /// gets its own gallery entry instead of one generic "pending" shot.
    /// Uses `PendingStage::ReadingSelection` (#354 follow-up review, finding
    /// 3): `calculate_selection` reads the current text selection, not a
    /// screenshot, and its own honest stage is the simplest non-`AskingModel`
    /// one to put in the gallery.
    CardPendingReadingSelection,
    /// Issue #354.
    CardPendingAskingModelShort,
    /// Issue #354: a long model name, to show `pending_status`'s
    /// truncation.
    CardPendingAskingModelLong,
    /// Issue #354.
    CardPendingStillWorking,
    CardPreviewCalendar,
    CardPreviewFormLongLabels,
    /// The preview's title line replaced by an error message, drawn in the
    /// error colour (a number field holding text).
    CardPreviewError,
    /// #885: the setup cards a new user sees, which show their detail line.
    CardSetupNoModel,
    CardSetupOllamaDown,
    PaletteDefault,
    PaletteRouterSuggestion,
    PaletteNoMatches,
    PaletteNoModel,
    /// Issue #560: the Settings window from a default config, Advanced
    /// collapsed, scrolled to the top / bottom.
    SettingsTop,
    SettingsBottom,
    /// Issue #560: the same with Advanced expanded.
    SettingsAdvancedTop,
    SettingsAdvancedBottom,
    /// Issue #580: the region-select overlay over a synthetic backdrop, with a
    /// selection staged.
    RegionOverlay,
}

/// The full ordered list of states the gallery steps through. Order is the
/// Right/Left stepping order and the order states are screenshotted in.
#[cfg(any(debug_assertions, test))]
pub fn gallery_states() -> Vec<GalleryState> {
    use GalleryState::*;
    vec![
        CardAnswerShort,
        CardAnswerDifficultyLow,
        CardAnswerDifficultyMid,
        CardAnswerDifficultyHigh,
        CardAnswerDifficultyUltra,
        CardAnswerExpanded,
        CardAnswerFocused,
        CardError,
        CardWelcome,
        CardPendingReadingSelection,
        CardPendingAskingModelShort,
        CardPendingAskingModelLong,
        CardPendingStillWorking,
        CardPreviewCalendar,
        CardPreviewFormLongLabels,
        CardPreviewError,
        CardSetupNoModel,
        CardSetupOllamaDown,
        PaletteDefault,
        PaletteRouterSuggestion,
        PaletteNoMatches,
        PaletteNoModel,
        SettingsTop,
        SettingsBottom,
        SettingsAdvancedTop,
        SettingsAdvancedBottom,
        RegionOverlay,
    ]
}

/// A stable, filesystem-safe, human-readable name for a state -- used both
/// for the screenshot filename and for any future debug printing. Lowercase,
/// hyphen-separated, no spaces or characters Windows forbids in filenames.
#[cfg(any(debug_assertions, test))]
pub fn state_name(state: GalleryState) -> &'static str {
    use GalleryState::*;
    match state {
        CardAnswerShort => "card-answer-short",
        CardAnswerDifficultyLow => "card-answer-difficulty-low",
        CardAnswerDifficultyMid => "card-answer-difficulty-mid",
        CardAnswerDifficultyHigh => "card-answer-difficulty-high",
        CardAnswerDifficultyUltra => "card-answer-difficulty-ultra",
        CardAnswerExpanded => "card-answer-expanded",
        CardAnswerFocused => "card-answer-focused",
        CardError => "card-error",
        CardWelcome => "card-welcome",
        CardPendingReadingSelection => "card-pending-reading-selection",
        CardPendingAskingModelShort => "card-pending-asking-model-short",
        CardPendingAskingModelLong => "card-pending-asking-model-long",
        CardPendingStillWorking => "card-pending-still-working",
        CardPreviewCalendar => "card-preview-calendar",
        CardPreviewFormLongLabels => "card-preview-form-long-labels",
        CardPreviewError => "card-preview-error",
        CardSetupNoModel => "card-setup-no-model",
        CardSetupOllamaDown => "card-setup-ollama-down",
        PaletteDefault => "palette-default",
        PaletteRouterSuggestion => "palette-router-suggestion",
        PaletteNoMatches => "palette-no-matches",
        PaletteNoModel => "palette-no-model",
        SettingsTop => "settings-top",
        SettingsBottom => "settings-bottom",
        SettingsAdvancedTop => "settings-advanced-top",
        SettingsAdvancedBottom => "settings-advanced-bottom",
        RegionOverlay => "region-overlay",
    }
}

/// `<state-name>.png`, the file a `--screenshot <dir>` run writes for a
/// given state.
#[cfg(any(debug_assertions, test))]
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
    use crate::ui::region::Overlay;
    use crate::ui::settings::GalleryWindow;
    use std::time::Duration;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, HBITMAP, HDC,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PW_CLIENTONLY};
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_LEFT, VK_RIGHT};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetClientRect, GetMessageW, PeekMessageW, SendMessageW, TranslateMessage,
        MSG, PM_REMOVE, WM_KEYDOWN, WM_PRINT,
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
        let mut settings: Option<GalleryWindow> = None;
        let mut region: Option<Overlay> = None;
        let states = gallery_states();

        if args.matrix {
            if args.theme.is_some() || args.dpi.is_some() {
                anyhow::bail!("--matrix renders every theme and scale; drop --theme and --dpi");
            }
            let dir = args
                .screenshot_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("--matrix needs --screenshot <dir>"))?;
            std::fs::create_dir_all(dir)?;
            for state in &states {
                if follows_variants(*state) {
                    for theme_name in MATRIX_THEMES {
                        for percent in MATRIX_SCALES {
                            let variant = Variant {
                                theme: theme_from_arg(theme_name),
                                dpi: dpi_from_arg(&percent.to_string()),
                            };
                            let name = matrix_filename(state_name(*state), theme_name, percent);
                            shoot(
                                &mut card,
                                &mut palette,
                                &mut settings,
                                &mut region,
                                variant,
                                *state,
                                &dir.join(name),
                            )?;
                        }
                    }
                } else if follows_dpi_only(*state) {
                    for percent in MATRIX_SCALES {
                        let variant = Variant {
                            theme: None,
                            dpi: dpi_from_arg(&percent.to_string()),
                        };
                        let name = matrix_filename(state_name(*state), "system", percent);
                        shoot(
                            &mut card,
                            &mut palette,
                            &mut settings,
                            &mut region,
                            variant,
                            *state,
                            &dir.join(name),
                        )?;
                    }
                } else {
                    // Live theme, the window's own DPI: named for the DPI it
                    // really had, found out once the window exists.
                    let path = dir.join("pending.png");
                    let named = shoot(
                        &mut card,
                        &mut palette,
                        &mut settings,
                        &mut region,
                        Variant::default(),
                        *state,
                        &path,
                    )
                    .and_then(|hwnd| {
                        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
                        // 0 means the window is gone: a "0" file would lie.
                        anyhow::ensure!(dpi > 0, "no DPI for the {} window", state_name(*state));
                        let name =
                            matrix_filename(state_name(*state), "system", scale_percent(dpi));
                        std::fs::rename(&path, dir.join(name))?;
                        Ok(())
                    });
                    if named.is_err() {
                        let _ = std::fs::remove_file(&path);
                    }
                    named?;
                }
            }
            std::process::exit(0);
        }

        let mut variant = Variant::default();
        if let Some(name) = &args.theme {
            variant.theme = Some(theme_from_arg(name).ok_or_else(|| {
                anyhow::anyhow!("--theme {name:?} is not one of light, dark, high-contrast")
            })?);
        }
        if let Some(value) = &args.dpi {
            variant.dpi = Some(dpi_from_arg(value).ok_or_else(|| {
                anyhow::anyhow!("--dpi {value:?} is not a scale from 100 to 500 percent")
            })?);
        }

        if let Some(dir) = &args.screenshot_dir {
            std::fs::create_dir_all(dir)?;
            for state in &states {
                let path = dir.join(screenshot_filename(state_name(*state)));
                shoot(
                    &mut card,
                    &mut palette,
                    &mut settings,
                    &mut region,
                    variant,
                    *state,
                    &path,
                )?;
            }
            std::process::exit(0);
        }

        let mut idx = 0usize;
        apply_variant(&mut card, variant);
        let hwnd0 = show_state(
            &mut card,
            &mut palette,
            &mut settings,
            &mut region,
            variant,
            states[idx],
        )?;
        let _ = hwnd0;
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if msg.message == WM_KEYDOWN {
                let vk = msg.wParam.0 as u16;
                if vk == VK_RIGHT.0 && idx + 1 < states.len() {
                    idx += 1;
                    show_state(
                        &mut card,
                        &mut palette,
                        &mut settings,
                        &mut region,
                        variant,
                        states[idx],
                    )?;
                } else if vk == VK_LEFT.0 && idx > 0 {
                    idx -= 1;
                    show_state(
                        &mut card,
                        &mut palette,
                        &mut settings,
                        &mut region,
                        variant,
                        states[idx],
                    )?;
                }
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    /// The theme and DPI a state is drawn under. `None` follows the system.
    /// Applies to the card states and the region overlay (see
    /// [`follows_variants`]).
    #[derive(Debug, Clone, Copy, Default)]
    struct Variant {
        theme: Option<crate::ui::theme::Theme>,
        dpi: Option<u32>,
    }

    fn apply_variant(card: &mut Card, variant: Variant) {
        if let Some(theme) = variant.theme {
            card.force_theme_for_gallery(theme);
        }
        if let Some(dpi) = variant.dpi {
            card.force_dpi_for_gallery(dpi);
        }
    }

    /// Shows `state` under `variant`, lets it paint, and saves it to `path`.
    fn shoot(
        card: &mut Card,
        palette: &mut Palette,
        settings: &mut Option<GalleryWindow>,
        region: &mut Option<Overlay>,
        variant: Variant,
        state: GalleryState,
        path: &std::path::Path,
    ) -> anyhow::Result<HWND> {
        apply_variant(card, variant);
        let hwnd = show_state(card, palette, settings, region, variant, state)?;
        pump_and_wait(Duration::from_millis(120));
        print_window_png(hwnd, path, prints_with_wm_print(state))?;
        Ok(hwnd)
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
        let proposal = serde_json::json!({
            "fields": [
                {
                    "control_id": "ctl1",
                    "source": "profile",
                    "label": "Employer-provided legal first and middle name",
                    "value": "Jordan Alexander",
                    "sensitive": false
                },
                {
                    "control_id": "ctl2",
                    "source": "profile",
                    "label": "Mailing address, including apartment or unit number",
                    "value": "742 Evergreen Terrace, Apt 4B",
                    "sensitive": false
                },
                {
                    "control_id": "ctl3",
                    "source": "profile",
                    "label": "Social security number (last four digits only)",
                    "value": "6789",
                    "sensitive": true
                }
            ]
        });
        // Built the way a real form fill is, so the labels are real labels.
        crate::actions::fill_form::build_preview_schema_and_value(&proposal)
    }

    fn free_palette_catalogue() -> Vec<PaletteAction> {
        vec![
            PaletteAction {
                id: "check-work".to_string(),
                name: "Check my work".to_string(),
                group: Some("Answer".to_string()),
                requires_model: true,
                effect: crate::ui::palette_model::Effect::None,
            },
            PaletteAction {
                id: "summarize".to_string(),
                name: "Summarize this".to_string(),
                group: Some("Answer".to_string()),
                requires_model: true,
                effect: crate::ui::palette_model::Effect::None,
            },
            PaletteAction {
                id: crate::ui::palette_model::CALCULATE_SELECTION_ACTION_ID.to_string(),
                name: "Calculate selection".to_string(),
                group: Some("Utilities".to_string()),
                requires_model: false,
                effect: crate::ui::palette_model::Effect::None,
            },
            PaletteAction {
                id: crate::ui::palette_model::COPY_REGION_ACTION_ID.to_string(),
                name: "Copy region to clipboard".to_string(),
                group: Some("Utilities".to_string()),
                requires_model: false,
                effect: crate::ui::palette_model::Effect::None,
            },
        ]
    }

    fn show_state(
        card: &mut Card,
        palette: &mut Palette,
        settings: &mut Option<GalleryWindow>,
        region: &mut Option<Overlay>,
        variant: Variant,
        state: GalleryState,
    ) -> anyhow::Result<HWND> {
        use GalleryState::*;
        *settings = None;
        *region = None;
        let instance = unsafe {
            windows::Win32::Foundation::HINSTANCE(
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?.0,
            )
        };
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
            CardAnswerExpanded | CardAnswerFocused => {
                card.hide();
                palette.hide();
                card.show_model_answer(
                    "The limit is 1/2",
                    "Apply L'Hopital's rule once and simplify.",
                    0,
                    Some(Difficulty::Level(5)),
                );
                card.expand_for_gallery();
                if state == CardAnswerFocused {
                    card.focus_first_link_for_gallery();
                }
            }
            CardError => {
                card.hide();
                palette.hide();
                card.show_error(
                    "Couldn't reach the model",
                    "The request timed out. Check your connection and try again.",
                );
            }
            CardSetupNoModel | CardSetupOllamaDown => {
                card.hide();
                palette.hide();
                let mut providers = crate::config::Providers {
                    order: vec!["openai".to_string()],
                    ..Default::default()
                };
                if state == CardSetupOllamaDown {
                    providers.order = vec!["ollama".to_string()];
                    providers.ollama.base_url = "http://127.0.0.1:11434".to_string();
                }
                let (headline, detail) = crate::app::empty_chain_card(
                    crate::mode::Mode::Auto,
                    &providers,
                    "config.toml",
                );
                card.show_settings_needed(&headline, &detail);
            }
            CardWelcome => {
                card.hide();
                palette.hide();
                let (headline, detail) = crate::app::welcome_card("Ctrl+Shift+/");
                card.show_settings_needed(&headline, &detail);
            }
            CardPendingReadingSelection => {
                card.hide();
                palette.hide();
                card.show_pending(crate::ui::card::PendingStage::ReadingSelection);
            }
            CardPendingAskingModelShort => {
                card.hide();
                palette.hide();
                card.show_pending(crate::ui::card::PendingStage::AskingModel {
                    model_name: "claude-haiku".to_string(),
                });
            }
            CardPendingAskingModelLong => {
                card.hide();
                palette.hide();
                card.show_pending(crate::ui::card::PendingStage::AskingModel {
                    // Deliberately over `pending_status`'s truncation
                    // length, to show the ellipsis in the gallery shot.
                    model_name:
                        "compat:some-very-long-self-hosted-provider-name-with-a-huge-model-id"
                            .to_string(),
                });
            }
            CardPendingStillWorking => {
                card.hide();
                palette.hide();
                card.show_pending(crate::ui::card::PendingStage::AskingModel {
                    model_name: "claude-haiku".to_string(),
                });
                card.set_pending_stage(crate::ui::card::PendingStage::StillWorking);
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
            CardPreviewError => {
                card.hide();
                palette.hide();
                let (schema, value) = calendar_fixture();
                let _ = card.show_preview("Add to calendar", &schema, &value, false);
                card.set_preview_error_for_gallery("Start must be a time, like 09:00.");
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
                    options: vec![crate::router::RouterOption {
                        intent: "check-work".to_string(),
                        label: "Check my work".to_string(),
                        confidence: 0.92,
                    }],
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
            RegionOverlay => {
                card.hide();
                palette.hide();
                // Without `--theme` it follows the live system theme; without
                // `--dpi` the hint is drawn at 96.
                let theme = variant.theme.unwrap_or_else(crate::ui::theme::detect_theme);
                let dpi = variant.dpi.unwrap_or(96);
                *region = Some(Overlay::open_for_gallery(instance, theme, dpi)?);
            }
            SettingsTop | SettingsBottom | SettingsAdvancedTop | SettingsAdvancedBottom => {
                card.hide();
                palette.hide();
                let mut w = GalleryWindow::open(instance, variant.dpi)
                    .ok_or_else(|| anyhow::anyhow!("could not open the Settings window"))?;
                w.set_advanced(matches!(
                    state,
                    SettingsAdvancedTop | SettingsAdvancedBottom
                ));
                w.scroll_to_end(matches!(state, SettingsBottom | SettingsAdvancedBottom));
                *settings = Some(w);
            }
        }
        Ok(match state {
            PaletteDefault | PaletteRouterSuggestion | PaletteNoMatches | PaletteNoModel => {
                palette.hwnd()
            }
            RegionOverlay => region
                .as_ref()
                .map(|o| o.hwnd())
                .ok_or_else(|| anyhow::anyhow!("no region overlay"))?,
            SettingsTop | SettingsBottom | SettingsAdvancedTop | SettingsAdvancedBottom => settings
                .as_ref()
                .map(|w| w.hwnd())
                .ok_or_else(|| anyhow::anyhow!("no Settings window"))?,
            _ => card.hwnd(),
        })
    }

    /// Captures `hwnd`'s client area and saves it as a PNG at `path`, with
    /// `PrintWindow` or, for `wm_print`, by sending `WM_PRINT` (see
    /// [`prints_with_wm_print`]). A specific application window, never the
    /// monitor (rule: this must never call anything from `capture.rs`, which
    /// grabs the screen for the real ask flow).
    fn print_window_png(hwnd: HWND, path: &std::path::Path, wm_print: bool) -> anyhow::Result<()> {
        unsafe {
            let mut rect = windows::Win32::Foundation::RECT::default();
            GetClientRect(hwnd, &mut rect)?;
            let width = (rect.right - rect.left).max(1);
            let height = (rect.bottom - rect.top).max(1);

            let screen_dc: HDC = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            let bitmap: HBITMAP = CreateCompatibleBitmap(screen_dc, width, height);
            let old = SelectObject(mem_dc, bitmap.into());

            if wm_print && !windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd).as_bool()
            {
                // A palette that hid itself (it hides when focus leaves it)
                // would print as a black page: fail loudly instead.
                anyhow::bail!("the window is hidden, so {path:?} would be a black page");
            }
            let ok = if wm_print {
                // PRF_CHECKVISIBLE | PRF_CHILDREN | PRF_CLIENT | PRF_ERASEBKGND
                SendMessageW(
                    hwnd,
                    WM_PRINT,
                    Some(WPARAM(mem_dc.0 as usize)),
                    Some(LPARAM(0x1 | 0x4 | 0x8 | 0x10)),
                );
                windows::core::BOOL(1)
            } else {
                PrintWindow(hwnd, mem_dc, PW_CLIENTONLY)
            };

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
                // GDI drawing leaves alpha at 0, which a PNG viewer shows as
                // a blank page (Settings, palette). The window is opaque.
                px[3] = 255;
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
                screenshot_dir: None,
                ..Default::default()
            })
        );
    }

    #[test]
    fn ui_gallery_with_a_dpi_override_and_the_matrix_flag() {
        let parsed = parse_gallery_args(&args(&["--ui-gallery", "--dpi", "150", "--matrix"]))
            .expect("--ui-gallery is present");
        assert_eq!(parsed.dpi.as_deref(), Some("150"));
        assert!(parsed.matrix);
        // A trailing --dpi with no value is absent, like --theme.
        let parsed = parse_gallery_args(&args(&["--ui-gallery", "--dpi"])).unwrap();
        assert_eq!((parsed.dpi, parsed.matrix), (None, false));
    }

    #[test]
    fn dpi_values_are_percent_scales_and_a_typo_maps_to_none() {
        assert_eq!(dpi_from_arg("100"), Some(96));
        assert_eq!(dpi_from_arg("125"), Some(120));
        assert_eq!(dpi_from_arg("150%"), Some(144));
        assert_eq!(dpi_from_arg("200"), Some(192));
        assert_eq!(dpi_from_arg("250"), Some(240));
        for bad in ["", "96", "99", "501", "abc", "1.5", "-100", "150 "] {
            assert_eq!(dpi_from_arg(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn every_matrix_scale_round_trips_through_its_dpi() {
        for pct in MATRIX_SCALES {
            let dpi = dpi_from_arg(&pct.to_string()).unwrap();
            assert_eq!(scale_percent(dpi), pct);
        }
        assert_eq!(scale_percent(96), 100);
    }

    #[test]
    fn matrix_filenames_are_predictable_and_unique_across_the_matrix() {
        assert_eq!(
            matrix_filename("card-error", "dark", 150),
            "card-error--dark--150.png"
        );
        for t in MATRIX_THEMES {
            assert!(theme_from_arg(t).is_some(), "{t} is not a --theme name");
        }
        let mut files = std::collections::HashSet::new();
        let mut count = 0;
        for state in gallery_states()
            .into_iter()
            .filter(|s| follows_variants(*s))
        {
            for t in MATRIX_THEMES {
                for pct in MATRIX_SCALES {
                    files.insert(matrix_filename(state_name(state), t, pct));
                    count += 1;
                }
            }
        }
        assert!(count > 0);
        assert_eq!(files.len(), count);
    }

    #[test]
    fn only_the_palette_and_settings_states_ignore_variants() {
        assert!(follows_variants(GalleryState::CardError));
        assert!(follows_variants(GalleryState::RegionOverlay));
        assert!(!follows_variants(GalleryState::PaletteDefault));
        assert!(!follows_variants(GalleryState::SettingsTop));
        for state in gallery_states() {
            let n = state_name(state);
            assert_eq!(
                follows_variants(state),
                !(n.starts_with("palette-") || n.starts_with("settings-")),
                "{n}"
            );
        }
    }

    #[test]
    fn only_the_palette_states_are_printed_with_wm_print() {
        let mut palette_states = 0;
        for state in gallery_states() {
            let n = state_name(state);
            assert_eq!(
                prints_with_wm_print(state),
                n.starts_with("palette-"),
                "{n}"
            );
            palette_states += usize::from(n.starts_with("palette-"));
        }
        assert_eq!(palette_states, 4, "the test must see every palette state");
    }

    #[test]
    fn ui_gallery_with_a_theme_override() {
        let parsed = parse_gallery_args(&args(&["--ui-gallery", "--theme", "high-contrast"]))
            .expect("--ui-gallery is present");
        assert_eq!(parsed.theme.as_deref(), Some("high-contrast"));
        // A trailing --theme with no value is absent, like --screenshot.
        let parsed = parse_gallery_args(&args(&["--ui-gallery", "--theme"])).unwrap();
        assert_eq!(parsed.theme, None);
    }

    #[test]
    fn theme_names_map_to_themes_and_a_typo_maps_to_none() {
        use crate::ui::theme::Theme;
        assert_eq!(theme_from_arg("light"), Some(Theme::Light));
        assert_eq!(theme_from_arg("dark"), Some(Theme::Dark));
        assert_eq!(theme_from_arg("high-contrast"), Some(Theme::HighContrast));
        assert_eq!(theme_from_arg("hc"), None);
        assert_eq!(theme_from_arg(""), None);
    }

    #[test]
    fn ui_gallery_with_screenshot_dir() {
        assert_eq!(
            parse_gallery_args(&args(&["--ui-gallery", "--screenshot", "out"])),
            Some(GalleryArgs {
                screenshot_dir: Some(PathBuf::from("out")),
                ..Default::default()
            })
        );
    }

    #[test]
    fn screenshot_with_no_following_arg_is_treated_as_absent() {
        assert_eq!(
            parse_gallery_args(&args(&["--ui-gallery", "--screenshot"])),
            Some(GalleryArgs {
                screenshot_dir: None,
                ..Default::default()
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
                screenshot_dir: Some(PathBuf::from("C:\\out dir")),
                ..Default::default()
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
        // 5 card-answer variants (short + 4 difficulty badges) + error + 4
        // pending sub-stages (#354) + 3 previews (calendar, long labels,
        // the error title) + the expanded answer, with and without link
        // focus = 15 card states, + 4 palette states + 4 settings states
        // (#560) + the region overlay (#580) + the welcome card (#831) = 25,
        // + the two setup cards (#885) = 27.
        assert_eq!(states.len(), 27);
        let names: std::collections::HashSet<&str> =
            states.iter().map(|s| state_name(*s)).collect();
        assert_eq!(
            names.len(),
            states.len(),
            "every gallery state must have a unique name"
        );
    }

    #[test]
    fn the_settings_states_are_listed_in_scroll_and_advanced_pairs() {
        let names: Vec<&str> = gallery_states().into_iter().map(state_name).collect();
        for want in [
            "settings-top",
            "settings-bottom",
            "settings-advanced-top",
            "settings-advanced-bottom",
        ] {
            assert!(names.contains(&want), "missing {want}");
        }
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
