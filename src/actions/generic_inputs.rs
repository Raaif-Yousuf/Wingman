//! What a generic (`actions.toml`) action sends to the model, decided from
//! its `inputs` list (#484). Pure logic, so it is unit-tested without a
//! window: `App::run_generic_action`'s worker does the gathering (selection,
//! clipboard, UIA) for the kinds [`wants_selection`] and friends say are
//! listed, then [`plan`] turns what it got into the request's user text and
//! says whether the screenshot goes along.
//!
//! The rule, in one line: the text inputs a contributor listed are sent as
//! text; the screenshot is sent only when `screen`/`window`/`region` is
//! listed (or no text input is) AND no listed text input produced anything.
//! That is the "falls back to the screen if nothing is selected" behaviour
//! `CONTRIBUTING.md`'s `translate-selection` example promises.
//!
//! `text` and `context` are accepted in the list but gathered by nothing
//! yet; they neither add text nor count as a reason to send the screen.
//! `window` and `region` are treated as `screen`: the capture is the active
//! monitor either way.

use super::InputKind;

/// The default user turn for a screenshot-only request, unchanged from
/// before #484.
pub const SCREEN_USER_TEXT: &str = "Look at the screen and respond.";

/// The text each listed text input produced. `None` means not requested or
/// nothing usable (blank, unreadable, a password field).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gathered {
    pub selection: Option<String>,
    pub clipboard: Option<String>,
    pub uia: Option<String>,
}

/// What the request carries: the user turn's text, and whether a
/// screenshot is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestInputs {
    pub user: String,
    pub screen: bool,
}

pub fn wants_selection(inputs: &[InputKind]) -> bool {
    inputs.contains(&InputKind::Selection)
}

pub fn wants_clipboard(inputs: &[InputKind]) -> bool {
    inputs.contains(&InputKind::Clipboard)
}

pub fn wants_uia(inputs: &[InputKind]) -> bool {
    inputs.contains(&InputKind::Uia)
}

fn wants_any_text(inputs: &[InputKind]) -> bool {
    wants_selection(inputs) || wants_clipboard(inputs) || wants_uia(inputs)
}

fn wants_screen(inputs: &[InputKind]) -> bool {
    inputs
        .iter()
        .any(|k| matches!(k, InputKind::Screen | InputKind::Window | InputKind::Region))
}

/// Decides the request's contents. `Err` is the card text for "the action
/// asked for text and there was none, and it did not ask for the screen".
pub fn plan(inputs: &[InputKind], gathered: &Gathered) -> Result<RequestInputs, String> {
    let mut sections = Vec::new();
    if let Some(t) = usable(&gathered.selection).filter(|_| wants_selection(inputs)) {
        sections.push(format!("Selected text:\n{t}"));
    }
    if let Some(t) = usable(&gathered.clipboard).filter(|_| wants_clipboard(inputs)) {
        sections.push(format!("Clipboard text:\n{t}"));
    }
    if let Some(t) = usable(&gathered.uia).filter(|_| wants_uia(inputs)) {
        sections.push(format!("Window fields:\n{t}"));
    }

    if !sections.is_empty() {
        return Ok(RequestInputs {
            user: sections.join("\n\n"),
            screen: false,
        });
    }
    // Nothing gathered. Send the screen when the action asked for it, or
    // asked for no text input at all (the pre-#484 behaviour, and what an
    // empty or `text`/`context`-only list still means).
    if wants_screen(inputs) || !wants_any_text(inputs) {
        return Ok(RequestInputs {
            user: SCREEN_USER_TEXT.to_string(),
            screen: true,
        });
    }
    Err(missing_message(inputs))
}

fn usable(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

/// Says what was missing, naming the first requested text input.
fn missing_message(inputs: &[InputKind]) -> String {
    if wants_selection(inputs) {
        "Nothing is selected. Select some text first (password fields are never read).".to_string()
    } else if wants_clipboard(inputs) {
        "The clipboard has no text. Copy some text first.".to_string()
    } else {
        "Couldn't read any text from the window.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use InputKind::*;

    fn g(sel: Option<&str>, clip: Option<&str>, uia: Option<&str>) -> Gathered {
        Gathered {
            selection: sel.map(String::from),
            clipboard: clip.map(String::from),
            uia: uia.map(String::from),
        }
    }

    #[test]
    fn selection_only_sends_the_text_and_no_screen() {
        let r = plan(&[Selection], &g(Some("hola"), None, None)).unwrap();
        assert_eq!(r.user, "Selected text:\nhola");
        assert!(!r.screen);
    }

    #[test]
    fn clipboard_only_sends_the_text_and_no_screen() {
        let r = plan(&[Clipboard], &g(None, Some("copied"), None)).unwrap();
        assert_eq!(r.user, "Clipboard text:\ncopied");
        assert!(!r.screen);
    }

    #[test]
    fn uia_only_sends_the_fields_and_no_screen() {
        let r = plan(&[Uia], &g(None, None, Some("Name: edit"))).unwrap();
        assert_eq!(r.user, "Window fields:\nName: edit");
        assert!(!r.screen);
    }

    #[test]
    fn selection_with_screen_prefers_the_selection() {
        let r = plan(&[Selection, Screen], &g(Some("hola"), None, None)).unwrap();
        assert_eq!(r.user, "Selected text:\nhola");
        assert!(!r.screen, "the screen is only the fallback");
    }

    #[test]
    fn selection_with_screen_falls_back_to_the_screen_when_nothing_selected() {
        for sel in [None, Some(""), Some("  \n ")] {
            let r = plan(&[Selection, Screen], &g(sel, None, None)).unwrap();
            assert_eq!(r.user, SCREEN_USER_TEXT);
            assert!(r.screen);
        }
    }

    #[test]
    fn several_text_inputs_are_all_sent_in_a_fixed_order() {
        let r = plan(
            &[Uia, Clipboard, Selection],
            &g(Some("s"), Some("c"), Some("u")),
        )
        .unwrap();
        assert_eq!(
            r.user,
            "Selected text:\ns\n\nClipboard text:\nc\n\nWindow fields:\nu"
        );
    }

    #[test]
    fn text_that_was_not_requested_is_never_sent() {
        let r = plan(&[Selection], &g(Some("s"), Some("secret"), Some("u"))).unwrap();
        assert_eq!(r.user, "Selected text:\ns");
    }

    #[test]
    fn screen_only_and_empty_lists_keep_the_old_request() {
        for inputs in [vec![Screen], vec![Window], vec![Region], vec![]] {
            let r = plan(&inputs, &Gathered::default()).unwrap();
            assert_eq!(r.user, SCREEN_USER_TEXT, "{inputs:?}");
            assert!(r.screen, "{inputs:?}");
        }
    }

    #[test]
    fn text_and_context_alone_are_not_gathered_so_the_screen_is_sent() {
        for inputs in [vec![Text], vec![Context], vec![Text, Context]] {
            let r = plan(&inputs, &Gathered::default()).unwrap();
            assert!(r.screen, "{inputs:?}");
        }
    }

    #[test]
    fn missing_text_without_a_screen_input_is_an_error_naming_what_is_missing() {
        let e = plan(&[Selection], &Gathered::default()).unwrap_err();
        assert!(e.contains("Nothing is selected"), "{e}");
        let e = plan(&[Clipboard], &Gathered::default()).unwrap_err();
        assert!(e.contains("clipboard has no text"), "{e}");
        let e = plan(&[Uia], &Gathered::default()).unwrap_err();
        assert!(e.contains("any text from the window"), "{e}");
    }

    #[test]
    fn error_texts_have_no_em_dash() {
        for inputs in [[Selection], [Clipboard], [Uia]] {
            let e = plan(&inputs, &Gathered::default()).unwrap_err();
            assert!(!e.contains('\u{2014}'));
        }
    }

    #[test]
    fn gathered_text_of_an_unlisted_kind_does_not_rescue_a_missing_one() {
        // Clipboard has text but the action only asked for the selection.
        let e = plan(&[Selection], &g(None, Some("c"), None)).unwrap_err();
        assert!(e.contains("Nothing is selected"));
    }
}
