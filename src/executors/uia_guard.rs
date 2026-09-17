//! The shared UIA-targeting deny-list (#31, for #32/#33). No executor may
//! target a UI element whose name or automation id reads as a final-action
//! button: CLAUDE.md's app description is explicit that "Wingman never
//! presses Send, Submit, Buy or Pay", and the expansion plan's "rules every
//! executor obeys" adds Place order and Checkout. Neither `replace_text`
//! (#32) nor `fill_form` (#33) exists yet -- this is a pure function with no
//! UIA dependency, so the *signature* future executors are written against
//! is pinned now, before there is any UIA code to retrofit it into.
//!
//! **Known gap:** only the English terms named above are covered, and a
//! deny term split across two separate words with a space where this
//! module expects one ("Check out", not "Checkout") is not caught either.
//! Localized variants (a German "Kaufen" button, a French "Payer") are not
//! caught. Filed as issue #204 for #32/#33 to pick up alongside their first
//! real UIA call, since neither exists yet to test against a real
//! localized app.

/// Deny terms that are a single word: matched as a whole *token* after
/// [`tokenize`] splits the candidate on non-alphanumeric characters AND on
/// camelCase/PascalCase boundaries. Element names are human-readable labels
/// ("Sender", "Payment method") where whole-word matching avoids false
/// positives; automation ids are usually camelCase or snake_case
/// identifiers with no spaces ("btnSend", "SUBMIT_BUTTON") where the same
/// tokenizer still finds the intended word. Case-insensitive throughout.
// Unused outside its own tests until #32/#33 exist to call it -- this
// module ships the signature and the pure logic ahead of any real UIA call
// (see the module doc comment).
#[allow(dead_code)]
const SINGLE_WORD_DENY: &[&str] = &["send", "submit", "buy", "pay"];

/// Deny terms that are a phrase: matched as a case-insensitive substring of
/// the whole candidate (not tokenized), because "Place Order" and
/// "Checkout" are meant to be read as a unit, and requiring an exact
/// two-token match would miss "PlaceOrderButton" or "checkout-btn".
#[allow(dead_code)]
const PHRASE_DENY: &[&str] = &["place order", "checkout"];

/// Whether a UI element with this name or automation id is off-limits for
/// any executor to target. Checked against both: either can carry the label
/// a real app puts on a button (see the expansion plan's `inputs/uia.rs`
/// row: UIA elements expose both Name and AutomationId).
#[allow(dead_code)]
pub fn is_forbidden_target(element_name: &str, automation_id: &str) -> bool {
    candidate_denied(element_name) || candidate_denied(automation_id)
}

#[allow(dead_code)]
fn candidate_denied(candidate: &str) -> bool {
    let normalized = candidate.to_lowercase();

    if PHRASE_DENY.iter().any(|phrase| normalized.contains(phrase)) {
        return true;
    }

    let tokens = tokenize(candidate);
    SINGLE_WORD_DENY
        .iter()
        .any(|term| tokens.iter().any(|token| token == term))
}

/// Splits `s` into lowercase alphanumeric tokens, breaking on any
/// non-alphanumeric character and on every lowercase-to-uppercase
/// transition (so "btnSend" tokenizes the same as "btn_send" or "btn send":
/// `["btn", "send"]`), so a whole-word check works the same way whether the
/// source is a human label or a camelCase automation id.
#[allow(dead_code)]
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_was_lowercase = false;

    for c in s.chars() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            prev_was_lowercase = false;
            continue;
        }
        if c.is_uppercase() && prev_was_lowercase && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        prev_was_lowercase = c.is_lowercase();
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- table test: denied ------------------------------------------------

    #[test]
    fn denies_every_term_the_task_names_case_insensitively() {
        let cases: &[(&str, &str)] = &[
            // (element_name, automation_id)
            ("Send", ""),
            ("SUBMIT", ""),
            ("Buy Now", ""),
            ("pay", ""),
            ("Pay Now", ""),
            ("Place Order", ""),
            ("place order", ""),
            ("Checkout", ""),
            ("", "btnSend"),
            ("", "SUBMIT_BUTTON"),
            ("", "buy-now"),
            ("", "pay_button"),
            ("", "SendButton"),
            ("", "checkout"),
        ];
        for (name, id) in cases {
            assert!(
                is_forbidden_target(name, id),
                "expected ({name:?}, {id:?}) to be denied"
            );
        }
    }

    // -- table test: allowed (no false positives on common field names) ---

    #[test]
    fn allows_ordinary_field_names_that_merely_contain_a_deny_substring() {
        let cases: &[(&str, &str)] = &[
            ("Sender", ""),
            ("Payment method", ""),
            ("Display name", ""),
            ("Buyer reference", ""),
            ("First name", ""),
            ("Comments", ""),
            ("Email", ""),
            ("", "txtSenderName"),
            ("", "displayName"),
        ];
        for (name, id) in cases {
            assert!(
                !is_forbidden_target(name, id),
                "expected ({name:?}, {id:?}) to be allowed"
            );
        }
    }

    // -- documented gap ------------------------------------------------------

    #[test]
    fn known_gap_two_word_check_out_is_not_caught() {
        // "checkout" (one word) is denied; "check out" (two words, a real
        // near-miss some sites use) is not, because it is neither a whole
        // token match nor a substring of the "checkout" phrase. Recorded
        // here so a future contributor sees this is known, not forgotten.
        assert!(!is_forbidden_target("Check out these items", ""));
    }
}
