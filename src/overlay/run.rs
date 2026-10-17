//! Running a detector over one capture: OCR the image, describe the space that
//! same image sits in, run the detector, keep what fits the cap. Runs on a
//! worker thread; OCR is injected so the tests never touch the real engine.
//!
//! The one thing that must hold: the OCR boxes and the [`Space`] describe the
//! same image. `ocr::recognize` returns boxes in the pixels of the image it
//! was given (undoing its own downscale to the engine's maximum), so the image
//! handed to it, its width and height, and the scale the capture recorded for
//! it (above 1 once `fit_for_model` has shrunk it) all come from one
//! [`RawShot`]. A test pins that with a shrunk shot.

use anyhow::{anyhow, Result};

use crate::capture::{RawShot, ScreenRect};
use crate::ocr::OcrOutput;

use super::anchor::Space;
use super::annotation::Annotation;
use super::detect;

/// What a detector found on one capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub annotations: Vec<Annotation>,
    /// The part of the screen the capture covered.
    pub monitor: ScreenRect,
}

/// Runs `detector` over `raw`. `recognize` gets exactly `raw`, so the boxes it
/// returns are in `raw`'s own pixels. `Err` for a capture that does not say
/// where on the screen it came from (nothing could be placed), or an OCR
/// failure.
pub fn detect_on_shot(
    raw: &RawShot,
    detector: &str,
    max_annotations: usize,
    recognize: &dyn Fn(&RawShot) -> Result<OcrOutput>,
) -> Result<Found> {
    let map = raw
        .screen
        .as_ref()
        .ok_or_else(|| anyhow!("the capture does not say where it sits on the screen"))?;
    let space = Space::for_image(raw.width, raw.height, map)
        .ok_or_else(|| anyhow!("the capture's screen position is not usable"))?;
    let ocr = recognize(raw)?;
    let mut annotations = detect::run(detector, &ocr.lines, &space);
    annotations.truncate(max_annotations.max(1));
    Ok(Found {
        annotations,
        monitor: space.monitor_rect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::ScreenMap;
    use crate::ocr::{OcrLine, OcrWord};

    fn shot(w: u32, h: u32, map: Option<ScreenMap>) -> RawShot {
        RawShot {
            rgba: vec![0; (w * h * 4) as usize],
            width: w,
            height: h,
            screen: map,
        }
    }

    fn empty() -> OcrOutput {
        OcrOutput {
            lines: Vec::new(),
            language_tag: "en-US".to_string(),
        }
    }

    fn one_word(text: &str, x: f32, y: f32, w: f32, h: f32) -> OcrOutput {
        let word = OcrWord {
            text: text.to_string(),
            x,
            y,
            width: w,
            height: h,
        };
        OcrOutput {
            lines: vec![OcrLine {
                text: text.to_string(),
                x,
                y,
                width: w,
                height: h,
                words: vec![word],
            }],
            language_tag: "en-US".to_string(),
        }
    }

    #[test]
    fn the_recognizer_sees_the_same_image_the_space_describes() {
        // A 1600x900 monitor at x=1920, shrunk to 800x450 before OCR.
        let map = ScreenMap::for_capture(1920, 0, 1600, 900, 800, 450, None);
        let raw = shot(800, 450, Some(map));
        let seen = std::cell::Cell::new((0u32, 0u32));
        let found = detect_on_shot(&raw, detect::MARK_WORD, 20, &|r| {
            seen.set((r.width, r.height));
            Ok(one_word("wonderful", 100.0, 50.0, 90.0, 10.0))
        })
        .unwrap();
        assert_eq!(seen.get(), (800, 450), "OCR ran on the shrunk image");
        // The box in shrunk pixels lands on native screen pixels: x doubled
        // and offset by the monitor's origin.
        assert_eq!(
            found.annotations[0].rects,
            vec![ScreenRect {
                left: 2120,
                top: 100,
                right: 2300,
                bottom: 120
            }]
        );
        assert_eq!(
            found.monitor,
            ScreenRect {
                left: 1920,
                top: 0,
                right: 3520,
                bottom: 900
            }
        );
    }

    #[test]
    fn an_unscaled_capture_maps_one_to_one() {
        let map = ScreenMap::for_capture(0, 0, 1000, 600, 1000, 600, None);
        let raw = shot(1000, 600, Some(map));
        let found = detect_on_shot(&raw, detect::MARK_WORD, 20, &|_| {
            Ok(one_word("wonderful", 10.0, 20.0, 90.0, 10.0))
        })
        .unwrap();
        assert_eq!(found.annotations[0].rects[0].left, 10);
        assert_eq!(found.annotations[0].rects[0].top, 20);
    }

    #[test]
    fn a_capture_with_no_screen_position_is_an_error_not_a_guess() {
        let raw = shot(10, 10, None);
        let called = std::cell::Cell::new(false);
        let r = detect_on_shot(&raw, detect::MARK_WORD, 20, &|_| {
            called.set(true);
            Ok(empty())
        });
        assert!(r.is_err());
        assert!(!called.get(), "no OCR is spent on a shot nothing can place");
    }

    #[test]
    fn an_unusable_scale_is_an_error() {
        let mut map = ScreenMap::for_capture(0, 0, 100, 100, 100, 100, None);
        map.scale_x = 0.0;
        let raw = shot(100, 100, Some(map));
        assert!(detect_on_shot(&raw, detect::MARK_WORD, 20, &|_| Ok(empty())).is_err());
    }

    #[test]
    fn an_ocr_failure_comes_back_as_an_error() {
        let map = ScreenMap::for_capture(0, 0, 100, 100, 100, 100, None);
        let raw = shot(100, 100, Some(map));
        let r = detect_on_shot(&raw, detect::MARK_WORD, 20, &|_| Err(anyhow!("no engine")));
        assert!(r.is_err());
    }

    #[test]
    fn nothing_found_is_an_empty_list() {
        let map = ScreenMap::for_capture(0, 0, 100, 100, 100, 100, None);
        let raw = shot(100, 100, Some(map));
        let found = detect_on_shot(&raw, detect::MARK_WORD, 20, &|_| Ok(empty())).unwrap();
        assert!(found.annotations.is_empty());
    }

    #[test]
    fn the_cap_is_applied_and_never_zero() {
        let map = ScreenMap::for_capture(0, 0, 1000, 600, 1000, 600, None);
        let raw = shot(1000, 600, Some(map));
        // The debug detector marks one word, so a cap of 0 still allows it.
        let found = detect_on_shot(&raw, detect::MARK_WORD, 0, &|_| {
            Ok(one_word("wonderful", 10.0, 20.0, 90.0, 10.0))
        })
        .unwrap();
        assert_eq!(found.annotations.len(), 1);
    }
}
