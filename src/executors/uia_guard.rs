//! The shared UIA-targeting deny-list (#31, for #32/#33). No executor may
//! target a UI element whose name or automation id reads as a final-action
//! button: AGENTS.md's app description is explicit that "Wingman never
//! presses Send, Submit, Buy or Pay", and the expansion plan's "rules every
//! executor obeys" adds Place order and Checkout. Neither `replace_text`
//! (#32) nor `fill_form` (#33) exists yet -- this is a pure function with no
//! UIA dependency, so the *signature* future executors are written against
//! is pinned now, before there is any UIA code to retrofit it into.
//!
//! **Scope: invokable elements only.** As with the English terms above,
//! every term in [`SINGLE_WORD_DENY`]/[`PHRASE_DENY`] (including the
//! localized ones added for #204) is meant to be checked against a
//! candidate INVOKABLE element -- a button, menu item, hyperlink or similar
//! control's Name/AutomationId -- never against arbitrary on-screen or
//! document text. A promotional sentence like "Check out our new feature"
//! IS a string this function would now flag if it were ever run against
//! page body text; that is out of scope by contract, not a bug, because no
//! current or planned executor calls this against non-invokable content
//! (see the framing above: the guard exists for what an executor might
//! click, not for text it merely reads). The caller -- not this function --
//! is responsible for only ever passing it the Name/AutomationId of an
//! element it is about to invoke.
//!
//! **#204 (closed):** added the two-word "Check out" phrase (previously a
//! documented gap: neither a whole-token match nor a substring of
//! "checkout") and localized single-word/phrase terms for German, French,
//! Spanish, Italian, Portuguese and Dutch, normalized case- and
//! accent-insensitively (see [`strip_diacritics`]). This is deliberately
//! not exhaustive -- it covers the specific terms #204 named, not a full
//! translation of every e-commerce/messaging verb in each language -- so a
//! new localized false negative remains possible; extend the tables below
//! if one is found. One accepted trade-off: a handful of these localized
//! single words (e.g. French "Payer") coincide with unrelated English
//! vocabulary (an insurance "Payer" field) and would now be denied there
//! too -- accepted because the deny-list only ever gates an invokable
//! element per the paragraph above, and a false deny (skip a legitimate
//! click) is a far cheaper mistake than a false allow (press a real
//! Pay/Payer button).

/// Deny terms that are a single word: matched as a whole *token* after
/// [`tokenize`] splits the candidate on non-alphanumeric characters AND on
/// camelCase/PascalCase boundaries. Element names are human-readable labels
/// ("Sender", "Payment method") where whole-word matching avoids false
/// positives; automation ids are usually camelCase or snake_case
/// identifiers with no spaces ("btnSend", "SUBMIT_BUTTON") where the same
/// tokenizer still finds the intended word. Case- and accent-insensitive
/// throughout (see [`strip_diacritics`]).
///
/// #204: extended with localized single-word terms -- German ("senden",
/// "kaufen", "bezahlen", "bestellen"), French ("envoyer", "acheter",
/// "payer", "commander"), Spanish/Portuguese ("enviar", "comprar", "pagar"
/// -- identical spellings in both languages), Italian ("invia", "acquista",
/// "paga") and Dutch ("verzenden", "kopen", "betalen", "afrekenen"). See the
/// module doc comment's "#204 (closed)" paragraph for scope and the
/// accepted cross-language false-positive trade-off.
// Unused outside its own tests until #32/#33 exist to call it -- this
// module ships the signature and the pure logic ahead of any real UIA call
// (see the module doc comment).
#[allow(dead_code)]
const SINGLE_WORD_DENY: &[&str] = &[
    "send",
    "submit",
    "buy",
    "pay", // English
    "senden",
    "kaufen",
    "bezahlen",
    "bestellen", // German
    "envoyer",
    "acheter",
    "payer",
    "commander", // French
    "enviar",
    "comprar",
    "pagar", // Spanish / Portuguese
    "invia",
    "acquista",
    "paga", // Italian
    "verzenden",
    "kopen",
    "betalen",
    "afrekenen", // Dutch
];

/// Deny terms that are a phrase: matched as a case-/accent-insensitive
/// substring of the whole candidate (not tokenized), because "Place Order"
/// and "Checkout" are meant to be read as a unit, and requiring an exact
/// multi-token match would miss "PlaceOrderButton" or "checkout-btn".
///
/// #204: added the two-word "check out" (previously a documented gap: a
/// button literally labelled "Check out" matched neither a single-word
/// token nor the "checkout" substring), plus two localized phrases --
/// German "jetzt kaufen" ("buy now") and Portuguese "finalizar compra"
/// ("complete purchase" / checkout).
#[allow(dead_code)]
const PHRASE_DENY: &[&str] = &[
    "place order",
    "checkout",
    "check out",
    "jetzt kaufen",
    "finalizar compra",
];

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
    let normalized = normalize(candidate);

    if PHRASE_DENY.iter().any(|phrase| normalized.contains(phrase)) {
        return true;
    }

    let tokens = tokenize(candidate);
    SINGLE_WORD_DENY
        .iter()
        .any(|term| tokens.iter().any(|token| token == term))
}

/// Lowercases `s` and folds each character through [`strip_diacritics`] --
/// the shared case-/accent-insensitive normalization used by both
/// [`candidate_denied`]'s phrase check and [`tokenize`]'s per-token check
/// (#204: deny terms are matched accent-insensitively, e.g. an accented
/// respelling of "payer" still matches "payer").
#[allow(dead_code)]
fn normalize(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .map(strip_diacritics)
        .collect()
}

/// Folds a small set of Latin accented letters -- common across this
/// module's target languages (English, German, French, Spanish, Italian,
/// Portuguese, Dutch) -- to their base ASCII letter. Not a full Unicode NFD
/// decomposition (no such dependency is worth adding here, see Cargo.toml's
/// dependency list): covers the accented letters that actually occur in
/// these languages' everyday vocabulary, not every possible diacritic.
/// Anything not in the table (including `ß`, which does not fold to a
/// single character -- a true fold is "ss", a whole-string transform this
/// per-`char` function cannot express) passes through unchanged.
#[allow(dead_code)]
fn strip_diacritics(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        other => other,
    }
}

/// Splits `s` into lowercase, accent-folded alphanumeric tokens, breaking on
/// any non-alphanumeric character and on every lowercase-to-uppercase
/// transition (so "btnSend" tokenizes the same as "btn_send" or "btn send":
/// `["btn", "send"]`), so a whole-word check works the same way whether the
/// source is a human label or a camelCase automation id. Accent-folding
/// (#204) happens per input character, before the uppercase-transition
/// check, so an accented letter's OWN case still counts for the camelCase
/// boundary the same way its unaccented equivalent would.
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
        current.extend(c.to_lowercase().map(strip_diacritics));
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

    // -- two-word "check out" (issue #204, closes the pre-existing gap) ----

    #[test]
    fn two_word_check_out_is_now_caught() {
        // Was the documented gap: "checkout" (one word) was denied but
        // "check out" (two words, a real near-miss some sites use) was not,
        // because it was neither a whole token match nor a substring of the
        // "checkout" phrase. #204 adds "check out" to PHRASE_DENY to close
        // it. This test replaces the old
        // `known_gap_two_word_check_out_is_not_caught`, which asserted the
        // opposite (the bug itself).
        assert!(is_forbidden_target("Check out", ""));
        assert!(is_forbidden_target("check out now", ""));
    }

    // -- localized deny terms (issue #204) ---------------------------------

    #[test]
    fn denies_localized_single_word_terms_case_and_accent_insensitively() {
        let cases: &[(&str, &str)] = &[
            // German
            ("Senden", "de single word"),
            ("KAUFEN", "de single word uppercase"),
            ("Bezahlen", "de single word"),
            ("Bestellen", "de single word"),
            // French
            ("Envoyer", "fr single word"),
            ("Acheter", "fr single word"),
            ("Payer", "fr single word"),
            ("Commander", "fr single word"),
            // Spanish / Portuguese (identical spellings)
            ("Enviar", "es/pt single word"),
            ("Comprar", "es/pt single word"),
            ("Pagar", "es/pt single word"),
            // Italian
            ("Invia", "it single word"),
            ("Acquista", "it single word"),
            ("Paga", "it single word"),
            // Dutch
            ("Verzenden", "nl single word"),
            ("Kopen", "nl single word"),
            ("Betalen", "nl single word"),
            ("Afrekenen", "nl single word"),
        ];
        for (name, label) in cases {
            assert!(
                is_forbidden_target(name, ""),
                "expected {name:?} ({label}) to be denied"
            );
            // Case-insensitive in both directions: also try lowercase and
            // fully uppercase spellings.
            assert!(
                is_forbidden_target(&name.to_lowercase(), ""),
                "expected lowercase {name:?} ({label}) to be denied"
            );
            assert!(
                is_forbidden_target(&name.to_uppercase(), ""),
                "expected uppercase {name:?} ({label}) to be denied"
            );
        }
    }

    #[test]
    fn denies_localized_phrase_terms() {
        let cases: &[(&str, &str)] = &[
            ("Jetzt kaufen", "de phrase"),
            ("JETZT KAUFEN", "de phrase uppercase"),
            ("Finalizar compra", "pt phrase"),
        ];
        for (name, label) in cases {
            assert!(
                is_forbidden_target(name, ""),
                "expected {name:?} ({label}) to be denied"
            );
        }
    }

    #[test]
    fn localized_terms_also_caught_in_automation_id_form() {
        // Mirrors the English table's camelCase/snake_case coverage: an
        // automation id carries the same word with no spaces.
        let cases: &[&str] = &["btnSenden", "KAUFEN_BUTTON", "payer-btn", "afrekenen"];
        for id in cases {
            assert!(
                is_forbidden_target("", id),
                "expected automation id {id:?} to be denied"
            );
        }
    }

    #[test]
    fn allows_ordinary_localized_field_names_that_merely_contain_a_deny_substring() {
        // Same false-positive concern as the English table: a field label
        // that happens to share a substring with a deny term (but is not
        // that whole token) must not be denied. German "Absender" (sender)
        // and "Versenden" (to send) both contain "senden" as a substring but
        // tokenize to a different whole word.
        let cases: &[(&str, &str)] = &[
            ("Absender", "de: sender (field label)"),
            ("Versender", "de: shipper (field label)"),
            ("Zahlungsart", "de: payment method (section header)"),
            ("Expéditeur", "fr: sender (field label, has an accent)"),
            ("Comprador", "es: buyer (field label)"),
        ];
        for (name, label) in cases {
            assert!(
                !is_forbidden_target(name, ""),
                "expected {name:?} ({label}) to be allowed"
            );
        }
    }

    // -- accent-insensitive normalization (issue #204) ----------------------

    #[test]
    fn strip_diacritics_folds_common_accented_letters_to_their_ascii_base() {
        let cases: &[(char, char)] = &[
            ('á', 'a'),
            ('à', 'a'),
            ('â', 'a'),
            ('ä', 'a'),
            ('ã', 'a'),
            ('é', 'e'),
            ('è', 'e'),
            ('ê', 'e'),
            ('ë', 'e'),
            ('í', 'i'),
            ('ï', 'i'),
            ('ó', 'o'),
            ('ö', 'o'),
            ('õ', 'o'),
            ('ú', 'u'),
            ('ü', 'u'),
            ('ç', 'c'),
            ('ñ', 'n'),
            // Unaffected: plain ASCII passes through unchanged.
            ('a', 'a'),
            ('Z', 'Z'),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_diacritics(*input), *expected, "failed for {input:?}");
        }
    }

    #[test]
    fn accented_variant_of_a_deny_term_is_still_caught() {
        // No required deny term itself carries an accent, so this proves
        // accent-folding end to end with a plausible accented respelling of
        // a real deny word rather than only unit-testing strip_diacritics
        // in isolation.
        assert!(is_forbidden_target("Páy", ""));
        assert!(is_forbidden_target("Envóyer", ""));
    }
}
