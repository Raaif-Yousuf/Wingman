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
    /// `--only <prefix>`: draw only the states whose name starts with this
    /// (`tray-menu`, `card-options`), so one surface can be looked at without
    /// rendering the whole gallery.
    pub only: Option<String>,
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
/// states, the palette, the region overlay and the tray menu can. Settings
/// follows the DPI only (see [`follows_dpi_only`]).
#[cfg(any(test, debug_assertions))]
pub fn follows_variants(state: GalleryState) -> bool {
    use GalleryState::*;
    !matches!(
        state,
        SettingsGeneral
            | SettingsModels
            | SettingsModelsClaude
            | SettingsModelsMoreProviders
            | SettingsPrivacy
            | SettingsActions
            | SettingsAdvanced
            | SettingsAdvancedPromptOpen
            | SettingsSaved
            | SettingsInvalid
            | SettingsNotSaved
            | SettingsKeyChecking
            | SettingsKeyRejected
    )
}

/// Whether a state is captured by sending `WM_PRINT` instead of calling
/// `PrintWindow`. The palette draws with a Direct2D window render target,
/// which `PrintWindow` cannot read, so its pictures were a black page (#626).
/// `WM_PRINT` gets the GDI paint.
/// The card and Settings paint with GDI and `PrintWindow` is exact for them.
#[cfg(any(test, debug_assertions))]
pub fn prints_with_wm_print(state: GalleryState) -> bool {
    use GalleryState::*;
    matches!(
        state,
        PaletteDefault
            | PaletteNoMatches
            | PaletteNoModel
            | PaletteHover
            | PaletteLong
            | PaletteLongScrolled
            | PaletteLongThumbHot
            | PaletteLongName
    )
}

/// Whether a state also gets a Direct2D picture. The palette draws with a
/// Direct2D window render target that a screen capture and `PrintWindow` cannot
/// read (#987), so its `WM_PRINT` picture is the GDI paint. The gallery
/// therefore paints it a second time through the real Direct2D code into an
/// off-screen target, saved next to the GDI one (see [`d2d_filename`]).
#[cfg(any(test, debug_assertions))]
pub fn draws_with_direct2d(state: GalleryState) -> bool {
    prints_with_wm_print(state)
}

/// The Direct2D picture's file for the GDI picture at `gdi`:
/// `palette-default--dark--150.png` becomes `palette-default--dark--150--d2d.png`.
#[cfg(any(test, debug_assertions))]
pub fn d2d_filename(gdi: &std::path::Path) -> PathBuf {
    let stem = gdi.file_stem().unwrap_or_default().to_string_lossy();
    gdi.with_file_name(format!("{stem}--d2d.png"))
}

/// Whether a state follows `--dpi` but not `--theme`: Settings can be laid out
/// at a simulated scale, but it has no forced theme yet (issue #662), so
/// `--matrix` draws it once per scale as `<state>--system--<percent>.png`.
#[cfg(any(test, debug_assertions))]
pub fn follows_dpi_only(state: GalleryState) -> bool {
    use GalleryState::*;
    matches!(
        state,
        SettingsGeneral
            | SettingsModels
            | SettingsModelsClaude
            | SettingsModelsMoreProviders
            | SettingsPrivacy
            | SettingsActions
            | SettingsAdvanced
            | SettingsAdvancedPromptOpen
            | SettingsSaved
            | SettingsInvalid
            | SettingsNotSaved
            | SettingsKeyChecking
            | SettingsKeyRejected
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
    let mut only = None;
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
        } else if a == "--only" {
            if let Some(prefix) = iter.next() {
                only = Some(prefix.clone());
            }
        }
    }
    Some(GalleryArgs {
        screenshot_dir,
        theme,
        dpi,
        matrix,
        only,
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
    /// #818: the preview's "Do it" button as with the pointer over it, held
    /// down, and with keyboard focus; and the Edit outline button hovered.
    CardPreviewDoItHover,
    CardPreviewDoItPressed,
    CardPreviewDoItFocused,
    CardPreviewEditHover,
    CardPreviewFormLongLabels,
    /// The preview's title line replaced by an error message, drawn in the
    /// error colour (a number field holding text).
    CardPreviewError,
    /// Issue #307: the options card while the router works: the summary
    /// placeholder, no buttons yet.
    CardOptionsLooking,
    /// Issue #307: three options, the router's answer.
    CardOptionsReady,
    /// Issue #307: four options with long labels and a long summary, and
    /// option 1 drawn as the default (the key runs it).
    CardOptionsDefaultLong,
    /// Issue #307: no model is set up, so the static list of actions that
    /// need none.
    CardOptionsNoModel,
    /// #885: the setup cards a new user sees, which show their detail line.
    CardSetupNoModel,
    CardSetupOllamaDown,
    /// #1020: the first failure a new user meets, as a plain setup card.
    CardSetupKeyRejected,
    PaletteDefault,
    PaletteNoMatches,
    PaletteNoModel,
    /// #883: more actions than fit, at the top: a scroll thumb at the top.
    PaletteLong,
    /// #883: the same list scrolled to the end: the thumb at the bottom.
    PaletteLongScrolled,
    /// The scroll thumb widened, as with the pointer over its strip (#988).
    PaletteLongThumbHot,
    /// #1000: an action name and a footer line far wider than the row: both
    /// end in an ellipsis, clear of the scroll thumb.
    PaletteLongName,
    /// #882: the mouse over a row other than the selected one.
    PaletteHover,
    /// Issue #560, #872: the Settings window from a default config, one state
    /// per page of the rail.
    SettingsGeneral,
    SettingsModels,
    SettingsModelsClaude,
    SettingsModelsMoreProviders,
    SettingsPrivacy,
    SettingsActions,
    /// The Advanced page with the Check my work prompt folded away.
    SettingsAdvanced,
    /// The same with the prompt open.
    SettingsAdvancedPromptOpen,
    /// #875: the General page with "Saved: Card corner" and Undo in the footer.
    SettingsSaved,
    /// #875: a number that does not parse, on the status line.
    SettingsInvalid,
    /// #875: a write that failed, on the status line.
    SettingsNotSaved,
    /// #876: the Models page while a typed key is being checked.
    SettingsKeyChecking,
    /// #876: the Models page after a vendor refused a typed key.
    SettingsKeyRejected,
    /// Issue #580: the region-select overlay over a synthetic backdrop, with a
    /// selection staged.
    RegionOverlay,
    /// #809: the in-place overlay over a fixture page: three notes, one of
    /// them flipped below its text and one spanning a line break. Drawn
    /// straight to a PNG (no window), so a screenshot run only.
    OverlayNotes,
    /// #825: the real tray menu, drawn from fixture data.
    TrayMenu,
    /// #825: the same with a row selected and its submenu open.
    TrayMenuSubmenu,
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
        CardPreviewDoItHover,
        CardPreviewDoItPressed,
        CardPreviewDoItFocused,
        CardPreviewEditHover,
        CardPreviewFormLongLabels,
        CardPreviewError,
        CardOptionsLooking,
        CardOptionsReady,
        CardOptionsDefaultLong,
        CardOptionsNoModel,
        CardSetupNoModel,
        CardSetupOllamaDown,
        CardSetupKeyRejected,
        PaletteDefault,
        PaletteNoMatches,
        PaletteNoModel,
        PaletteLong,
        PaletteLongScrolled,
        PaletteLongThumbHot,
        PaletteLongName,
        PaletteHover,
        SettingsGeneral,
        SettingsModels,
        SettingsModelsClaude,
        SettingsModelsMoreProviders,
        SettingsPrivacy,
        SettingsActions,
        SettingsAdvanced,
        SettingsAdvancedPromptOpen,
        SettingsSaved,
        SettingsInvalid,
        SettingsNotSaved,
        SettingsKeyChecking,
        SettingsKeyRejected,
        RegionOverlay,
        OverlayNotes,
        TrayMenu,
        TrayMenuSubmenu,
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
        CardPreviewDoItHover => "card-preview-doit-hover",
        CardPreviewDoItPressed => "card-preview-doit-pressed",
        CardPreviewDoItFocused => "card-preview-doit-focused",
        CardPreviewEditHover => "card-preview-edit-hover",
        CardPreviewFormLongLabels => "card-preview-form-long-labels",
        CardPreviewError => "card-preview-error",
        CardOptionsLooking => "card-options-looking",
        CardOptionsReady => "card-options-ready",
        CardOptionsDefaultLong => "card-options-default-long",
        CardOptionsNoModel => "card-options-no-model",
        CardSetupNoModel => "card-setup-no-model",
        CardSetupOllamaDown => "card-setup-ollama-down",
        CardSetupKeyRejected => "card-setup-key-rejected",
        PaletteDefault => "palette-default",
        PaletteNoMatches => "palette-no-matches",
        PaletteNoModel => "palette-no-model",
        PaletteLong => "palette-long",
        PaletteLongScrolled => "palette-long-scrolled",
        PaletteLongThumbHot => "palette-long-thumb-hot",
        PaletteLongName => "palette-long-name",
        PaletteHover => "palette-hover",
        SettingsGeneral => "settings-general",
        SettingsModels => "settings-models",
        SettingsModelsClaude => "settings-models-claude",
        SettingsModelsMoreProviders => "settings-models-more-providers",
        SettingsPrivacy => "settings-privacy",
        SettingsActions => "settings-actions",
        SettingsAdvanced => "settings-advanced",
        SettingsAdvancedPromptOpen => "settings-advanced-prompt-open",
        SettingsSaved => "settings-saved",
        SettingsInvalid => "settings-invalid",
        SettingsNotSaved => "settings-not-saved",
        SettingsKeyChecking => "settings-key-checking",
        SettingsKeyRejected => "settings-key-rejected",
        RegionOverlay => "region-overlay",
        OverlayNotes => "overlay-notes",
        TrayMenu => "tray-menu",
        TrayMenuSubmenu => "tray-menu-submenu",
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
// (verified by code inspection: this module never imports `hotkey` or
// `single_instance`, and it uses `tray` only through `Tray::for_gallery`,
// which builds the menu on a tray that never adds a shell icon), and never
// constructs anything from `provider::` that would make a real HTTP call -- every value handed to `show_answer`/
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
    use crate::ui::settings::{GalleryWindow, Page};
    use std::time::Duration;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, HBITMAP, HDC,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PW_CLIENTONLY};
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_LEFT, VK_RIGHT};
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
        palette.keep_open_for_gallery();
        palette.ignore_mouse_for_test();
        let mut settings: Option<GalleryWindow> = None;
        let mut region: Option<Overlay> = None;
        let mut states = gallery_states();
        if let Some(prefix) = &args.only {
            states.retain(|s| state_name(*s).starts_with(prefix.as_str()));
            anyhow::ensure!(!states.is_empty(), "--only {prefix:?} matches no state");
        }

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
                    // Tray menus and the like: once, at the monitor's own DPI,
                    // named for the DPI the window really had.
                    let variant = Variant::default();
                    let path = dir.join("pending.png");
                    let named = shoot(
                        &mut card,
                        &mut palette,
                        &mut settings,
                        &mut region,
                        variant,
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
        apply_variant(&mut card, &mut palette, variant);
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

    fn apply_variant(card: &mut Card, palette: &mut Palette, variant: Variant) {
        if let Some(theme) = variant.theme {
            card.force_theme_for_gallery(theme);
            palette.force_theme_for_gallery(theme);
        }
        if let Some(dpi) = variant.dpi {
            card.force_dpi_for_gallery(dpi);
            palette.force_dpi_for_gallery(dpi);
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
        apply_variant(card, palette, variant);
        if state == GalleryState::OverlayNotes {
            // No window: the overlay is a layered window, which `PrintWindow`
            // renders black (#626). Its picture is the frame it would show.
            card.hide();
            palette.hide();
            crate::ui::overlay::gallery_png(variant.theme, variant.dpi.unwrap_or(96), path)?;
            return Ok(card.hwnd());
        } else if matches!(
            state,
            GalleryState::TrayMenu | GalleryState::TrayMenuSubmenu
        ) {
            // A real menu in its own modal loop, captured from the screen.
            // The card window only names the DPI the picture was drawn at.
            card.hide();
            palette.hide();
            tray_menu_shot(variant, state, Some(path))?;
            return Ok(card.hwnd());
        }
        let hwnd = show_state(card, palette, settings, region, variant, state)?;
        pump_and_wait(Duration::from_millis(120));
        if state == GalleryState::PaletteHover {
            // Windows sends a mouse move when a window appears under the
            // cursor, which clears the hover set above: set it again so the
            // picture does not depend on where the cursor happens to be.
            palette.set_hover_for_test(Some(2));
        }
        print_window_png(hwnd, path, prints_with_wm_print(state))?;
        if draws_with_direct2d(state) {
            direct2d_png(palette, &d2d_filename(path))?;
        }
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

    /// More actions than the palette shows at once, so it draws a scroll thumb.
    fn long_palette_catalogue() -> Vec<PaletteAction> {
        [
            "Check my work",
            "Summarize this",
            "Explain this",
            "Translate this",
            "Rewrite more simply",
            "Fix the grammar",
            "Copy text from screen",
            "Copy region to clipboard",
            "Calculate selection",
            "Convert units",
            "Convert times",
            "Add to calendar",
            "Fill this form",
            "Read a QR code",
            "Define this word",
            "Find similar images",
        ]
        .iter()
        .enumerate()
        .map(|(i, name)| PaletteAction {
            id: format!("long-{i}"),
            name: (*name).to_string(),
            group: None,
            requires_model: false,
            effect: crate::ui::palette_model::Effect::None,
            takes_text: false,
        })
        .collect()
    }

    /// One action whose name is about 90 characters, among short ones (#1000).
    fn long_name_palette_catalogue() -> Vec<PaletteAction> {
        [
            "Check my work",
            "Copy every table on this page into a spreadsheet and keep the column headings as they are",
            "Summarize this",
        ]
        .iter()
        .enumerate()
        .map(|(i, name)| PaletteAction {
            id: format!("long-name-{i}"),
            name: (*name).to_string(),
            group: None,
            requires_model: false,
            effect: crate::ui::palette_model::Effect::None,
            takes_text: false,
        })
        .collect()
    }

    /// The options card's rows from `(id, label)` pairs.
    fn option_rows(rows: &[(&str, &str)]) -> Vec<crate::ui::card::OptionRow> {
        rows.iter()
            .map(|(id, label)| crate::ui::card::OptionRow {
                id: (*id).to_string(),
                label: (*label).to_string(),
            })
            .collect()
    }

    /// The options card as the app shows it: the longest dismiss time (so the
    /// interactive gallery does not lose it) and the key claim on, so the
    /// hint line reads as it does in use.
    fn options_behavior(key_runs_first: bool) -> crate::ui::card::OptionsBehavior {
        crate::ui::card::OptionsBehavior {
            dismiss_secs: 120,
            claim_keys: true,
            key_runs_first,
            foreground: 0,
        }
    }

    fn free_palette_catalogue() -> Vec<PaletteAction> {
        vec![
            PaletteAction {
                id: "check-work".to_string(),
                name: "Check my work".to_string(),
                group: Some("Answer".to_string()),
                requires_model: true,
                effect: crate::ui::palette_model::Effect::None,
                takes_text: false,
            },
            PaletteAction {
                id: "summarize".to_string(),
                name: "Summarize this".to_string(),
                group: Some("Answer".to_string()),
                requires_model: true,
                effect: crate::ui::palette_model::Effect::None,
                takes_text: false,
            },
            PaletteAction {
                id: crate::ui::palette_model::CALCULATE_SELECTION_ACTION_ID.to_string(),
                name: "Calculate selection".to_string(),
                group: Some("Utilities".to_string()),
                requires_model: false,
                effect: crate::ui::palette_model::Effect::None,
                takes_text: false,
            },
            PaletteAction {
                id: crate::ui::palette_model::COPY_REGION_ACTION_ID.to_string(),
                name: "Copy region to clipboard".to_string(),
                group: Some("Utilities".to_string()),
                requires_model: false,
                effect: crate::ui::palette_model::Effect::None,
                takes_text: false,
            },
        ]
    }

    /// The Settings page a gallery state shows.
    fn settings_page(state: GalleryState) -> Page {
        match state {
            GalleryState::SettingsModels
            | GalleryState::SettingsModelsClaude
            | GalleryState::SettingsModelsMoreProviders
            | GalleryState::SettingsKeyChecking
            | GalleryState::SettingsKeyRejected => Page::Models,
            GalleryState::SettingsPrivacy => Page::Privacy,
            GalleryState::SettingsActions => Page::Actions,
            GalleryState::SettingsAdvanced | GalleryState::SettingsAdvancedPromptOpen => {
                Page::Advanced
            }
            _ => Page::General,
        }
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
                // Expanded and tracked, so the footer shows "Copy details"
                // and "Report a problem" (#373).
                card.show_error_with_details(
                    "Couldn't reach the model",
                    "The request timed out. Check your connection and try again.",
                );
                card.expand_for_gallery();
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
                    None,
                );
                card.show_settings_needed(&headline, &detail);
            }
            CardSetupKeyRejected => {
                card.hide();
                palette.hide();
                let raw = "anthropic: HTTP 401: invalid x-api-key";
                let (headline, detail) = crate::app::setup_failure(raw).unwrap_or_default();
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
            CardPreviewDoItHover
            | CardPreviewDoItPressed
            | CardPreviewDoItFocused
            | CardPreviewEditHover => {
                use crate::ui::card::{ButtonDemo, ID_PREVIEW_DO_IT, ID_PREVIEW_EDIT};
                card.hide();
                palette.hide();
                let (schema, value) = calendar_fixture();
                // The Edit button only exists when the main window does.
                let _ = card.show_preview(
                    "Add to calendar",
                    &schema,
                    &value,
                    state == CardPreviewEditHover,
                );
                card.set_button_demo_for_gallery(Some(match state {
                    CardPreviewDoItHover => (ID_PREVIEW_DO_IT, ButtonDemo::Hover),
                    CardPreviewDoItPressed => (ID_PREVIEW_DO_IT, ButtonDemo::Pressed),
                    CardPreviewDoItFocused => (ID_PREVIEW_DO_IT, ButtonDemo::Focus),
                    _ => (ID_PREVIEW_EDIT, ButtonDemo::Hover),
                }));
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
                palette.show(
                    free_palette_catalogue(),
                    true,
                    crate::ui::palette_model::footer_line("Auto", Some("openai:gpt-5.5")),
                );
            }
            CardOptionsLooking => {
                card.hide();
                palette.hide();
                card.show_options(
                    1,
                    crate::router::LOOKING_SUMMARY,
                    &[],
                    options_behavior(false),
                );
            }
            CardOptionsReady => {
                card.hide();
                palette.hide();
                card.show_options(
                    1,
                    "Compose window to Dana, \"Q3 numbers\"",
                    &option_rows(&[
                        ("review-this-email", "Review this email to Dana"),
                        ("rewrite-tone", "Make it sound friendlier"),
                        ("translate", "Translate to Spanish"),
                    ]),
                    options_behavior(false),
                );
            }
            CardOptionsDefaultLong => {
                card.hide();
                palette.hide();
                card.show_options(
                    1,
                    "A long thread about the quarterly numbers, with a table of figures and two attachments",
                    &option_rows(&[
                        (
                            "review-this-email",
                            "Review this email to Dana about the quarterly numbers",
                        ),
                        ("explain-screen", "Explain what is on screen"),
                        ("chart-table", "Turn the table into a chart"),
                        ("extract-text", "Copy text from screen"),
                    ]),
                    options_behavior(true),
                );
            }
            CardOptionsNoModel => {
                card.hide();
                palette.hide();
                card.show_options(
                    1,
                    crate::router::NO_MODEL_SUMMARY,
                    &option_rows(&[
                        ("extract-text", "Copy text from screen"),
                        ("read-qr", "Read QR code from screen"),
                        ("calculate-selection", "Calculate selection"),
                        (
                            crate::router::SETUP_MODEL_ID,
                            crate::router::SETUP_MODEL_LABEL,
                        ),
                    ]),
                    options_behavior(false),
                );
            }
            PaletteNoMatches => {
                card.hide();
                palette.hide();
                palette.show(
                    free_palette_catalogue(),
                    true,
                    crate::ui::palette_model::footer_line("Auto", Some("openai:gpt-5.5")),
                );
                palette.set_query_for_test("zzzzznomatch");
            }
            PaletteNoModel => {
                card.hide();
                palette.hide();
                palette.show(
                    free_palette_catalogue(),
                    false,
                    crate::ui::palette_model::footer_line("Auto", Some("openai:gpt-5.5")),
                );
            }
            PaletteLong | PaletteLongScrolled | PaletteLongThumbHot => {
                card.hide();
                palette.hide();
                palette.show(
                    long_palette_catalogue(),
                    true,
                    crate::ui::palette_model::footer_line("Auto", Some("openai:gpt-5.5")),
                );
                if state == PaletteLongScrolled {
                    palette.scroll_for_test(i32::MAX);
                }
                if state == PaletteLongThumbHot {
                    palette.set_thumb_hot_for_test(true);
                }
            }
            PaletteLongName => {
                card.hide();
                palette.hide();
                palette.show(
                    long_name_palette_catalogue(),
                    true,
                    crate::ui::palette_model::footer_line(
                        "Auto",
                        Some("openai:a-model-with-a-name-far-too-long-for-the-footer-line-to-hold-and-then-some-more-and-a-good-deal-more-after-that"),
                    ),
                );
            }
            PaletteHover => {
                card.hide();
                palette.hide();
                palette.show(
                    free_palette_catalogue(),
                    true,
                    crate::ui::palette_model::footer_line("Auto", Some("openai:gpt-5.5")),
                );
                // Row 0 is the "Answer" header, row 1 the selected action:
                // the hovered one is the action under it.
                palette.set_hover_for_test(Some(2));
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
            OverlayNotes => {
                // Screenshot runs only (see `shoot`): interactively there is
                // nothing to show, so the card and palette are just cleared.
                card.hide();
                palette.hide();
            }
            TrayMenu | TrayMenuSubmenu => {
                card.hide();
                palette.hide();
                // Interactive: the menu opens and stays until it is closed.
                tray_menu_shot(variant, state, None)?;
            }
            SettingsGeneral
            | SettingsModels
            | SettingsModelsClaude
            | SettingsModelsMoreProviders
            | SettingsPrivacy
            | SettingsActions
            | SettingsAdvanced
            | SettingsAdvancedPromptOpen
            | SettingsSaved
            | SettingsInvalid
            | SettingsNotSaved
            | SettingsKeyChecking
            | SettingsKeyRejected => {
                card.hide();
                palette.hide();
                let mut w = GalleryWindow::open(instance, variant.dpi)
                    .ok_or_else(|| anyhow::anyhow!("could not open the Settings window"))?;
                w.show_page(settings_page(state));
                w.set_advanced(matches!(state, SettingsAdvancedPromptOpen));
                match state {
                    SettingsModelsClaude => {
                        w.pick_provider(crate::config::ActiveProvider::Anthropic)
                    }
                    SettingsModelsMoreProviders => w.reveal_providers(),
                    // #875: the status line after a change (and Undo), a number
                    // that does not parse, and a write that failed.
                    SettingsSaved => w.show_status("Saved: Card corner", true),
                    SettingsInvalid => {
                        w.show_status("Error: Hide card after. Enter a whole number of seconds, such as 8.", false)
                    }
                    SettingsNotSaved => w.show_status(
                        "Not saved: Card corner. The disk is full. Wingman is still using the old value.",
                        false,
                    ),
                    // #876: a typed key being asked of its vendor, and refused.
                    SettingsKeyChecking => w.show_status("Checking key...", false),
                    SettingsKeyRejected => w.show_status(
                        "Error: OpenAI key. OpenAI did not accept that key. Check it and try again.",
                        false,
                    ),
                    _ => {}
                }
                *settings = Some(w);
            }
        }
        Ok(match state {
            PaletteDefault | PaletteNoMatches | PaletteNoModel | PaletteHover | PaletteLong
            | PaletteLongScrolled | PaletteLongThumbHot | PaletteLongName => palette.hwnd(),
            RegionOverlay => region
                .as_ref()
                .map(|o| o.hwnd())
                .ok_or_else(|| anyhow::anyhow!("no region overlay"))?,
            SettingsGeneral
            | SettingsModels
            | SettingsModelsClaude
            | SettingsModelsMoreProviders
            | SettingsPrivacy
            | SettingsActions
            | SettingsAdvanced
            | SettingsAdvancedPromptOpen
            | SettingsSaved
            | SettingsInvalid
            | SettingsNotSaved
            | SettingsKeyChecking
            | SettingsKeyRejected => settings
                .as_ref()
                .map(|w| w.hwnd())
                .ok_or_else(|| anyhow::anyhow!("no Settings window"))?,
            _ => card.hwnd(),
        })
    }

    /// Shows the real tray menu, built from fixture data on a tray with no
    /// shell icon, and, with `path`, saves a picture of it and closes it
    /// (#825). The menu runs its own modal loop, so a timer on the host
    /// window drives it: it presses the keys the state needs, then captures
    /// the menu windows from the screen. The process DPI is the monitor's, so
    /// the file is named for that scale.
    fn tray_menu_shot(
        variant: Variant,
        state: GalleryState,
        path: Option<&std::path::Path>,
    ) -> anyhow::Result<()> {
        use windows::core::w;
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
        };
        let instance = unsafe {
            windows::Win32::Foundation::HINSTANCE(
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?.0,
            )
        };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(menu_host_proc),
            hInstance: instance,
            lpszClassName: w!("WingmanGalleryMenuHost"),
            ..Default::default()
        };
        // Registering twice (a matrix run) fails harmlessly with "exists".
        unsafe { RegisterClassW(&wc) };
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("WingmanGalleryMenuHost"),
                w!("Wingman gallery menu host"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )
        }?;
        let tray = crate::ui::tray::Tray::for_gallery(hwnd, instance);
        let theme = variant.theme.unwrap_or_else(crate::ui::theme::detect_theme);
        let sys = if theme == crate::ui::theme::Theme::HighContrast {
            crate::ui::theme::HC_BLACK
        } else {
            crate::ui::theme::SysColors::read()
        };
        let menu = tray
            .prepare_menu_at(POINT { x: 80, y: 80 }, theme, sys, variant.dpi)
            .ok_or_else(|| anyhow::anyhow!("could not build the tray menu"))?;
        MENU_SHOT.with(|s| {
            *s.borrow_mut() = Some(MenuShot {
                path: path.map(|p| p.to_path_buf()),
                keys: if state == GalleryState::TrayMenuSubmenu {
                    // Down to "Mode" (the first press lands on the version line), then Right.
                    vec![VK_DOWN.0; 6].into_iter().chain([VK_RIGHT.0]).collect()
                } else {
                    Vec::new()
                },
                step: 0,
                error: None,
            })
        });
        if path.is_some() {
            unsafe { windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 1, 400, None) };
        }
        let _ = menu.track();
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        drop(tray);
        let shot = MENU_SHOT.with(|s| s.borrow_mut().take());
        match shot.and_then(|s| s.error) {
            Some(e) => Err(anyhow::anyhow!(e)),
            None => Ok(()),
        }
    }

    struct MenuShot {
        path: Option<std::path::PathBuf>,
        keys: Vec<u16>,
        step: u32,
        error: Option<String>,
    }

    thread_local! {
        static MENU_SHOT: std::cell::RefCell<Option<MenuShot>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The owner of the gallery's menu: answers the menu's owner-draw messages
    /// and runs the capture timer.
    unsafe extern "system" fn menu_host_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, EndMenu, KillTimer, SetTimer, WM_TIMER,
        };
        if let Some(answer) = crate::ui::tray_menu::handle_message(msg, wparam, lparam) {
            return answer;
        }
        if msg == WM_TIMER {
            let _ = KillTimer(Some(hwnd), 1);
            let (step, keys, path) = MENU_SHOT.with(|s| {
                let mut s = s.borrow_mut();
                match s.as_mut() {
                    Some(shot) => {
                        shot.step += 1;
                        (shot.step, shot.keys.clone(), shot.path.clone())
                    }
                    None => (0, Vec::new(), None),
                }
            });
            if step == 1 && !keys.is_empty() {
                for vk in keys {
                    press_key(vk);
                }
                SetTimer(Some(hwnd), 1, 400, None);
            } else {
                if let Some(path) = path {
                    if let Err(e) = capture_menus_png(&path) {
                        MENU_SHOT.with(|s| {
                            if let Some(shot) = s.borrow_mut().as_mut() {
                                shot.error = Some(e.to_string());
                            }
                        });
                    }
                }
                let _ = EndMenu();
            }
            return windows::Win32::Foundation::LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn press_key(vk: u16) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
            KEYEVENTF_KEYUP, VIRTUAL_KEY,
        };
        let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        unsafe {
            SendInput(
                &[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)],
                std::mem::size_of::<INPUT>() as i32,
            );
        }
        std::thread::sleep(Duration::from_millis(60));
    }

    /// Saves the screen area every open menu window covers as a PNG.
    fn capture_menus_png(path: &std::path::Path) -> anyhow::Result<()> {
        use windows::core::w;
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::{BitBlt, CAPTUREBLT, SRCCOPY};
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowExW, GetWindowRect, IsWindowVisible,
        };
        let mut union: Option<RECT> = None;
        let mut prev: Option<HWND> = None;
        unsafe {
            while let Ok(menu) = FindWindowExW(None, prev, w!("#32768"), None) {
                prev = Some(menu);
                if !IsWindowVisible(menu).as_bool() {
                    continue;
                }
                let mut r = RECT::default();
                if GetWindowRect(menu, &mut r).is_err() {
                    continue;
                }
                union = Some(match union {
                    None => r,
                    Some(u) => RECT {
                        left: u.left.min(r.left),
                        top: u.top.min(r.top),
                        right: u.right.max(r.right),
                        bottom: u.bottom.max(r.bottom),
                    },
                });
            }
        }
        let r = union.ok_or_else(|| anyhow::anyhow!("no menu window was on screen"))?;
        // A little margin so the shadow edge and corners can be seen.
        let (x, y) = (r.left - 8, r.top - 8);
        let (width, height) = (r.right - r.left + 16, r.bottom - r.top + 16);
        unsafe {
            let screen_dc: HDC = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            let bitmap: HBITMAP = CreateCompatibleBitmap(screen_dc, width, height);
            let old = SelectObject(mem_dc, bitmap.into());
            let blit = BitBlt(
                mem_dc,
                0,
                0,
                width,
                height,
                Some(screen_dc),
                x,
                y,
                SRCCOPY | CAPTUREBLT,
            );
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
            let lines = windows::Win32::Graphics::Gdi::GetDIBits(
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
            blit?;
            // GetDIBits returns the scan lines it copied; fewer than asked
            // for means the buffer is still zeroed and the PNG would be black.
            if lines != height {
                anyhow::bail!("GetDIBits copied {lines} of {height} lines for {path:?}");
            }
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2);
                px[3] = 255;
            }
            let img: image::RgbaImage =
                image::ImageBuffer::from_raw(width as u32, height as u32, buf)
                    .ok_or_else(|| anyhow::anyhow!("bad capture buffer for {path:?}"))?;
            img.save(path)
                .map_err(|e| anyhow::anyhow!("failed to save {path:?}: {e}"))?;
        }
        Ok(())
    }

    /// Captures `hwnd`'s client area and saves it as a PNG at `path`, with
    /// `PrintWindow` or, for `wm_print`, by sending `WM_PRINT` (see
    /// [`prints_with_wm_print`]). A specific application window, never the
    /// monitor (rule: this must never call anything from `capture.rs`, which
    /// grabs the screen for the real ask flow).
    fn print_window_png(hwnd: HWND, path: &std::path::Path, wm_print: bool) -> anyhow::Result<()> {
        if wm_print
            && !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd) }.as_bool()
        {
            // A palette that hid itself (it hides when focus leaves it)
            // would print as a black page: fail loudly instead.
            anyhow::bail!("the window is hidden, so {path:?} would be a black page");
        }
        capture_client_png(hwnd, path, |dc| {
            if wm_print {
                // PRF_CHECKVISIBLE | PRF_CHILDREN | PRF_CLIENT | PRF_ERASEBKGND
                unsafe {
                    SendMessageW(
                        hwnd,
                        WM_PRINT,
                        Some(WPARAM(dc.0 as usize)),
                        Some(LPARAM(0x1 | 0x4 | 0x8 | 0x10)),
                    );
                }
                Ok(())
            } else if unsafe { PrintWindow(hwnd, dc, PW_CLIENTONLY) }.as_bool() {
                Ok(())
            } else {
                anyhow::bail!("PrintWindow failed for {path:?}")
            }
        })
    }

    /// Saves the palette as painted by the real Direct2D code (#987): the
    /// paint list goes through `paint_ops_d2d` into a bitmap, with the query
    /// box printed on top (see `paint_d2d_into_for_gallery`).
    fn direct2d_png(palette: &mut Palette, path: &std::path::Path) -> anyhow::Result<()> {
        let hwnd = palette.hwnd();
        capture_client_png(hwnd, path, |dc| palette.paint_d2d_into_for_gallery(dc))
    }

    /// Makes a bitmap the size of `hwnd`'s client area, lets `fill` draw into
    /// its device context, and saves the result as a PNG at `path`.
    fn capture_client_png(
        hwnd: HWND,
        path: &std::path::Path,
        fill: impl FnOnce(HDC) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        unsafe {
            let mut rect = windows::Win32::Foundation::RECT::default();
            GetClientRect(hwnd, &mut rect)?;
            let width = (rect.right - rect.left).max(1);
            let height = (rect.bottom - rect.top).max(1);

            let screen_dc: HDC = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            let bitmap: HBITMAP = CreateCompatibleBitmap(screen_dc, width, height);
            let old = SelectObject(mem_dc, bitmap.into());

            let filled = fill(mem_dc);

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

            filled?;

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
    fn only_names_a_state_prefix() {
        let parsed = parse_gallery_args(&args(&["--ui-gallery", "--only", "tray-menu"]))
            .expect("--ui-gallery is present");
        assert_eq!(parsed.only.as_deref(), Some("tray-menu"));
        let names: Vec<&str> = gallery_states().into_iter().map(state_name).collect();
        assert_eq!(
            names.iter().filter(|n| n.starts_with("tray-menu")).count(),
            2,
            "the prefix finds the two tray menu states"
        );
        // A dangling --only is ignored, like a dangling --screenshot.
        let parsed = parse_gallery_args(&args(&["--ui-gallery", "--only"])).unwrap();
        assert_eq!(parsed.only, None);
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
    fn only_the_settings_states_ignore_variants() {
        assert!(follows_variants(GalleryState::CardError));
        assert!(follows_variants(GalleryState::RegionOverlay));
        assert!(follows_variants(GalleryState::TrayMenu));
        assert!(follows_variants(GalleryState::TrayMenuSubmenu));
        assert!(!follows_variants(GalleryState::SettingsGeneral));
        for state in gallery_states() {
            let n = state_name(state);
            assert_eq!(follows_variants(state), !n.starts_with("settings-"), "{n}");
        }
    }

    /// #1007: the palette is drawn at every matrix theme and scale, so its
    /// 100 and 150 percent pictures exist.
    #[test]
    fn the_palette_states_follow_theme_and_scale() {
        let mut palette_states = 0;
        for state in gallery_states() {
            let n = state_name(state);
            if n.starts_with("palette-") {
                assert!(follows_variants(state) && !follows_dpi_only(state), "{n}");
                palette_states += 1;
            }
        }
        assert_eq!(palette_states, 8, "the test must see every palette state");
    }

    /// #987: the Direct2D picture sits next to the GDI one, same name plus
    /// `--d2d`, so the two can be compared file by file.
    #[test]
    fn the_direct2d_picture_is_named_after_the_gdi_one() {
        let gdi = matrix_filename("palette-default", "dark", 150);
        assert_eq!(gdi, "palette-default--dark--150.png");
        assert_eq!(
            d2d_filename(std::path::Path::new("out").join(&gdi).as_path()),
            std::path::Path::new("out").join("palette-default--dark--150--d2d.png")
        );
        assert!(draws_with_direct2d(GalleryState::PaletteHover));
        assert!(!draws_with_direct2d(GalleryState::CardError));
        assert!(!draws_with_direct2d(GalleryState::SettingsGeneral));
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
        assert_eq!(palette_states, 8, "the test must see every palette state");
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
        // focus = 15 card states, + 4 options card states (#307) + 3 palette
        // states + 4 settings states (#560) + the region overlay (#580) + the
        // welcome card (#831) = 28, + the two setup cards (#885) = 30, + the
        // two long palettes (#883) = 33, + the tray menu with and without an
        // open submenu (#825) = 35, + the long action name (#1000) = 36, + the
        // key-rejected setup card (#1020) = 37, + the Settings states going
        // from four to one per page and the prompt open (#872) = 39, + the
        // Models page with Claude active and with every provider shown (#874)
        // = 41, + the palette with a widened thumb (#988) = 42, + the preview's
        // button states (#818) = 46, + the status line saved, not valid and not
        // saved (#875) = 49, + the in-place overlay (#809) = 50, + the key being
        // checked and refused (#876) = 52.
        assert_eq!(states.len(), 52);
        let names: std::collections::HashSet<&str> =
            states.iter().map(|s| state_name(*s)).collect();
        assert_eq!(
            names.len(),
            states.len(),
            "every gallery state must have a unique name"
        );
    }

    #[test]
    fn the_options_card_states_are_listed_and_follow_themes_and_scales() {
        let names: Vec<&str> = gallery_states().into_iter().map(state_name).collect();
        for want in [
            "card-options-looking",
            "card-options-ready",
            "card-options-default-long",
            "card-options-no-model",
        ] {
            assert!(names.contains(&want), "missing {want}");
        }
        assert!(follows_variants(GalleryState::CardOptionsReady));
        assert!(follows_variants(GalleryState::CardOptionsDefaultLong));
        assert!(
            !names.iter().any(|n| n.contains("router-suggestion")),
            "the palette no longer shows a router suggestion (#805)"
        );
    }

    #[test]
    fn every_settings_page_has_a_gallery_state() {
        let names: Vec<&str> = gallery_states().into_iter().map(state_name).collect();
        for want in [
            "settings-general",
            "settings-models",
            "settings-privacy",
            "settings-actions",
            "settings-advanced",
            "settings-advanced-prompt-open",
            "settings-saved",
            "settings-invalid",
            "settings-not-saved",
            "settings-key-checking",
            "settings-key-rejected",
        ] {
            assert!(names.contains(&want), "missing {want}");
        }
        // One state per page of the rail: a page added there needs a picture.
        assert_eq!(
            crate::ui::settings::page_labels().len(),
            names.iter().filter(|n| n.starts_with("settings-")).count() - 8,
            "the Advanced page has two states (prompt closed and open), the Models page three (#874) and the status line five (#875, #876)"
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
