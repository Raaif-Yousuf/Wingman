//! The one shared table of payment-shaped *label* terms (issue #215):
//! before this file, `executors::fill_form::is_payment_label` (a proposal
//! field's label, checked before ever writing it) and
//! `profile::denylist::contains_cvv_label`/`contains_bank_label` (a profile
//! field's name or value, checked on every save and load) each maintained
//! their own separately-worded term list for the same underlying concept
//! (AGENTS.md rule: never store or fill payment data). Filed as #215 the
//! night both landed in parallel with no shared vocabulary; this module is
//! the reconciliation: one canonical table per payment concept (card
//! number, expiry, IBAN, CVV, bank/routing), both call sites read from it,
//! and a term added to cover a gap in one caller is automatically covered
//! in the other.
//!
//! Deliberately dependency-free (no `regex`), matching
//! `profile::denylist`'s own reasoning: every check here is a plain
//! case-insensitive substring scan.
//!
//! # Value-shape checks (issue #220)
//!
//! Originally this module only reconciled *label*-shaped term matching: the
//! *value*-shape checks (Luhn-valid digit runs, mod-97-valid IBANs) lived
//! only in `profile::denylist`, on the theory that `executors::fill_form`
//! never needed them because it refuses by label, before a value is ever
//! written, and the profile has no payment fields to source one from in the
//! first place. That theory held for a `"profile"`-sourced value (it is
//! structurally guaranteed clean -- `Profile::save_to`/`load_from` already
//! refuse to store one) but not for a `"model"`-sourced value
//! (`actions::fill_form::merge_model_response`'s `"model"` arm): literal
//! text the model invented from a screenshot, with no such guarantee, that
//! could sit behind an ordinary-looking label ("Reference number",
//! "Confirmation code") and reach `executors::fill_form::evaluate_resolved_field`
//! unblocked, since that function also only checked the label. [`contains_card_number`]
//! and [`contains_iban`] (moved here from `profile::denylist`, which now
//! delegates to them) and [`is_payment_shaped_value`] close that gap: both
//! `actions::fill_form::merge_model_response`'s `"model"` arm and
//! `executors::fill_form::evaluate_resolved_field` call
//! [`is_payment_shaped_value`] directly, the same way both already called
//! [`is_payment_shaped_label`] for the label side (#215).

/// Card-number-shaped label wording.
pub const CARD_NUMBER_TERMS: &[&str] = &["card number"];

/// Expiry-date-shaped label wording.
pub const EXPIRY_TERMS: &[&str] = &["expiry", "expiration date"];

/// IBAN-shaped label wording.
pub const IBAN_TERMS: &[&str] = &["iban"];

/// CVV / card-verification-value label wording.
pub const CVV_TERMS: &[&str] = &[
    "cvv2",
    "cvc2",
    "cvv",
    "cvc",
    "security code",
    "card verification",
];

/// Bank account / routing label wording.
pub const BANK_TERMS: &[&str] = &[
    "account number",
    "acct number",
    "acct no",
    "account no",
    "routing number",
    "routing no",
    "aba routing",
    "sort code",
    "bank account",
    "swift code",
    "bic code",
];

/// Case-insensitive substring match of `text` against any term in `terms`.
pub fn contains_any(text: &str, terms: &[&str]) -> bool {
    let lower = text.to_lowercase();
    terms.iter().any(|t| lower.contains(t))
}

/// Whether `text` (a form field's label, or a profile field's name/value)
/// reads as any payment-shaped concept this table knows about: card
/// number, expiry, IBAN, CVV, or bank/routing. The single entry point both
/// `executors::fill_form::is_payment_label` and
/// `profile::denylist::check_field` call, so the two can never drift by a
/// term one caller added and the other forgot (#215's Done-when).
pub fn is_payment_shaped_label(text: &str) -> bool {
    contains_any(text, CARD_NUMBER_TERMS)
        || contains_any(text, EXPIRY_TERMS)
        || contains_any(text, IBAN_TERMS)
        || contains_any(text, CVV_TERMS)
        || contains_any(text, BANK_TERMS)
}

// ---------------------------------------------------------------------------
// Value-shape checks (issue #220): Luhn-valid card numbers, mod-97-valid
// IBANs. Moved here from `profile::denylist`, which now delegates to these
// -- see this file's module doc comment for why the value side needed the
// same reconciliation the label side got in #215.
// ---------------------------------------------------------------------------

/// True if `text` contains a run of digits, optionally separated by spaces
/// or hyphens, whose digit count is 12 to 19 (the range real card numbers
/// fall in) and which passes the Luhn checksum.
pub fn contains_card_number(text: &str) -> bool {
    for run in digit_runs_with_separators(text) {
        let len = run.len();
        if (12..=19).contains(&len) && luhn_valid(&run) {
            return true;
        }
    }
    false
}

/// Extracts maximal runs of `[0-9 -]` from `text`, returning just the
/// digits of each run (separators stripped), split wherever a character
/// outside that set appears. A run with fewer than 2 digits is not
/// emitted; a lone digit can never be a card number.
fn digit_runs_with_separators(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if ch == ' ' || ch == '-' {
            // separator inside a run: keep scanning, contributes no digit
        } else {
            if current.len() >= 2 {
                runs.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.len() >= 2 {
        runs.push(current);
    }
    runs
}

/// Standard Luhn checksum over an all-digit string. `pub(crate)`: only
/// `profile::denylist`'s own tests reach past [`contains_card_number`] to
/// this directly, to test the checksum in isolation from the digit-run
/// extraction around it.
pub(crate) fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for ch in digits.chars().rev() {
        let mut d = ch.to_digit(10).expect("digits() only yields ASCII digits");
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    sum % 10 == 0
}

/// True if `text` contains a substring shaped like a real IBAN (2 letters +
/// 2 check digits + 11-30 more alphanumerics, spaces allowed between every
/// 4 characters as IBANs are conventionally printed) that passes the
/// ISO 7064 MOD97-10 check.
pub fn contains_iban(text: &str) -> bool {
    for run in alnum_runs_with_spaces(text) {
        let len = run.len();
        if (15..=34).contains(&len) && iban_mod97_valid(&run) {
            return true;
        }
    }
    false
}

/// Byte ranges of every card number in `text`: the same rule as
/// [`contains_card_number`] (12 to 19 digits, optional space or hyphen
/// separators, Luhn-valid), but returning where each one sits so screenshot
/// redaction (#103) can paint just that part of an OCR line. Unlike the
/// boolean check, a number sitting after other digits in the same run
/// ("Total 100 4111 1111 1111 1111") is found: every window of digit groups
/// is tried, and every window that passes counts. Overlapping passing windows
/// are merged into one span, so no digit of a matched card is left out even
/// when other digits sit in front of it ("123456 4111 1111 1111 1111"): the
/// leading digits get painted too, which is the safe direction.
pub fn card_number_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    card_spans_where(text, |_| true)
}

/// Like [`card_number_spans`], but a number must also start with a digit a
/// real card network uses (3 to 6: Amex, Visa, Mastercard, Discover, plus Mastercard's 2221 to 2720). Free
/// text is full of Luhn-valid 16-digit runs that are not cards (timestamps,
/// tracking and order numbers), so the text path (#505) uses this stricter
/// rule. A run cannot span a line break: only digits, spaces and hyphens
/// join groups.
pub fn card_number_spans_with_known_prefix(text: &str) -> Vec<std::ops::Range<usize>> {
    card_spans_where(text, |digits| {
        matches!(digits.as_bytes()[0], b'3'..=b'6')
            // Mastercard's 2-series: first four digits 2221 to 2720.
            || digits
                .get(..4)
                .and_then(|p| p.parse::<u32>().ok())
                .is_some_and(|p| (2221..=2720).contains(&p))
    })
}

fn card_spans_where(text: &str, accept: impl Fn(&str) -> bool) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    for run in runs_of(text, |c| c.is_ascii_digit() || c == ' ' || c == '-') {
        let groups = groups_of(text, run, |c| c.is_ascii_digit());
        spans.extend(passing_windows(text, &groups, 19, |digits| {
            digits.len() >= 12 && luhn_valid(digits) && accept(digits)
        }));
    }
    merge_overlapping(spans)
}

/// Byte ranges of every IBAN in `text`: the same rule as [`contains_iban`],
/// with positions and merging as in [`card_number_spans`].
pub fn iban_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    for run in runs_of(text, |c| c.is_ascii_alphanumeric() || c == ' ') {
        let groups = groups_of(text, run, |c| c.is_ascii_alphanumeric());
        spans.extend(passing_windows(text, &groups, 34, |s| {
            s.len() >= 15 && iban_mod97_valid(s)
        }));
    }
    merge_overlapping(spans)
}

/// Every window of consecutive `groups` whose concatenated, upper-cased text
/// (at most `max_len` long) satisfies `valid`, as a byte range from the
/// window's first group to its last.
fn passing_windows(
    text: &str,
    groups: &[std::ops::Range<usize>],
    max_len: usize,
    valid: impl Fn(&str) -> bool,
) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    for i in 0..groups.len() {
        let mut joined = String::new();
        for g in &groups[i..] {
            joined.push_str(&text[g.clone()].to_ascii_uppercase());
            if joined.len() > max_len {
                break;
            }
            if valid(&joined) {
                out.push(groups[i].start..g.end);
            }
        }
    }
    out
}

/// Sorts `ranges` and joins any that overlap.
fn merge_overlapping(mut ranges: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start < last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Maximal byte ranges of `text` whose characters all satisfy `keep`.
fn runs_of(text: &str, keep: impl Fn(char) -> bool) -> Vec<std::ops::Range<usize>> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (keep(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push(s..text.len());
    }
    runs
}

/// Maximal sub-ranges of `run` whose characters satisfy `keep`.
fn groups_of(
    text: &str,
    run: std::ops::Range<usize>,
    keep: impl Fn(char) -> bool,
) -> Vec<std::ops::Range<usize>> {
    runs_of(&text[run.clone()], keep)
        .into_iter()
        .map(|r| run.start + r.start..run.start + r.end)
        .collect()
}

/// Extracts maximal runs of ASCII letters/digits from `text`, treating
/// spaces as separators that do not end a run (so `"GB29 NWBK 6016 1331
/// 9268 19"` becomes one run), split wherever any other character appears.
fn alnum_runs_with_spaces(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_uppercase());
        } else if ch == ' ' {
            // separator inside a run
        } else {
            if current.len() >= 4 {
                runs.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.len() >= 4 {
        runs.push(current);
    }
    runs
}

/// ISO 7064 MOD97-10: move the first 4 characters to the end, convert every
/// letter to two digits (A=10 .. Z=35), and check the resulting number is
/// congruent to 1 mod 97. Requires the first two characters to be letters
/// (the IBAN country code) and the next two to be digits (the IBAN check
/// digits) before attempting the arithmetic, so an arbitrary 15+ char
/// alphanumeric string (a long ID, a hex hash) is rejected up front rather
/// than by coincidentally failing the checksum.
fn iban_mod97_valid(candidate: &str) -> bool {
    let chars: Vec<char> = candidate.chars().collect();
    if chars.len() < 15 {
        return false;
    }
    if !chars[0].is_ascii_alphabetic() || !chars[1].is_ascii_alphabetic() {
        return false;
    }
    if !chars[2].is_ascii_digit() || !chars[3].is_ascii_digit() {
        return false;
    }

    let rearranged: String = chars[4..].iter().chain(chars[0..4].iter()).collect();

    // Fold each character into the running remainder mod 97: a digit
    // contributes one decimal digit, a letter contributes its two-digit
    // A=10..Z=35 value, one digit at a time so the running number never
    // needs to be materialized in full.
    let mut remainder: u64 = 0;
    for ch in rearranged.chars() {
        let value = if ch.is_ascii_digit() {
            ch.to_digit(10).unwrap() as u64
        } else if ch.is_ascii_alphabetic() {
            (ch as u64) - ('A' as u64) + 10
        } else {
            return false;
        };
        if value >= 10 {
            remainder = (remainder * 10 + value / 10) % 97;
            remainder = (remainder * 10 + value % 10) % 97;
        } else {
            remainder = (remainder * 10 + value) % 97;
        }
    }
    remainder == 1
}

/// Whether `text` contains a value that is shaped like a real payment
/// credential by STRUCTURE alone (a Luhn-valid card number, a mod-97-valid
/// IBAN), regardless of what the field around it is called. Complements
/// [`is_payment_shaped_label`]: a label check catches an honestly-labeled
/// payment field; this catches a payment-shaped value hiding behind an
/// innocuous label (#220) -- the one thing a `"profile"`-sourced value can
/// never be (`Profile::save_to`/`load_from` refuse to store one) but a
/// `"model"`-sourced value, or a user's own edit in the confirm-time
/// preview, has no such guarantee against.
pub fn is_payment_shaped_value(text: &str) -> bool {
    contains_card_number(text) || contains_iban(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the exact cases fill_form::is_payment_label used to assert alone --

    #[test]
    fn denies_every_term_fill_forms_own_table_used_to_carry() {
        let cases = [
            "Card number",
            "CVV",
            "cvc",
            "Expiry date",
            "expiration date",
            "IBAN",
            "Account number",
            "Routing number",
        ];
        for label in cases {
            assert!(
                is_payment_shaped_label(label),
                "expected {label:?} to be denied"
            );
        }
    }

    #[test]
    fn allows_the_exact_ordinary_labels_fill_forms_own_table_used_to_allow() {
        let cases = [
            "Email",
            "Display name",
            "Discount code",
            "Account settings",
            "Phone number",
            "Shipping address",
        ];
        for label in cases {
            assert!(
                !is_payment_shaped_label(label),
                "expected {label:?} to be allowed"
            );
        }
    }

    // -- the exact cases profile::denylist's CVV_LABELS/BANK_LABELS used to
    // assert alone -----------------------------------------------------

    #[test]
    fn denies_every_cvv_variant_profiles_own_table_used_to_carry() {
        assert!(is_payment_shaped_label("CVV: 123"));
        assert!(is_payment_shaped_label("cvc2 code"));
        assert!(is_payment_shaped_label("Security Code"));
        assert!(is_payment_shaped_label("card verification value"));
    }

    #[test]
    fn denies_every_bank_variant_profiles_own_table_used_to_carry() {
        assert!(is_payment_shaped_label("Account Number: 12345678"));
        assert!(is_payment_shaped_label("Routing Number 021000021"));
        assert!(is_payment_shaped_label("Sort code 12-34-56"));
        assert!(is_payment_shaped_label("SWIFT code ABCDEF12"));
    }

    #[test]
    fn allows_ordinary_profile_prose() {
        assert!(!is_payment_shaped_label(
            "Just a regular note about dinner plans."
        ));
        assert!(!is_payment_shaped_label(
            "Meet at the account desk for the conference badge."
        ));
    }

    // -- case/substring behaviour -------------------------------------------

    #[test]
    fn matching_is_case_insensitive() {
        assert!(is_payment_shaped_label("CARD NUMBER"));
        assert!(is_payment_shaped_label("card NUMBER"));
    }

    #[test]
    fn matching_is_a_substring_not_a_whole_string() {
        assert!(is_payment_shaped_label(
            "Please enter your card number below"
        ));
    }

    #[test]
    fn contains_any_finds_a_term_anywhere_in_the_text() {
        assert!(contains_any("prefix iban suffix", IBAN_TERMS));
        assert!(!contains_any("nothing relevant here", IBAN_TERMS));
    }

    // -- value-shape checks (#220) -------------------------------------------

    #[test]
    fn luhn_valid_test_visa_number_is_detected() {
        // A well-known Luhn-valid test card number (not a real account).
        assert!(luhn_valid("4111111111111111"));
    }

    #[test]
    fn luhn_invalid_number_is_not_detected() {
        assert!(!luhn_valid("4111111111111112"));
    }

    #[test]
    fn card_number_with_spaces_is_detected() {
        assert!(contains_card_number("4111 1111 1111 1111"));
    }

    #[test]
    fn card_number_with_hyphens_is_detected() {
        assert!(contains_card_number("4111-1111-1111-1111"));
    }

    #[test]
    fn short_digit_run_is_not_a_card_number() {
        assert!(!contains_card_number("12345"));
    }

    #[test]
    fn ordinary_phone_number_is_not_a_card_number() {
        assert!(!contains_card_number("+1 555-123-4567"));
    }

    #[test]
    fn non_luhn_digit_run_is_not_a_card_number() {
        // 16 digits, deliberately not Luhn-valid: an order number, not a
        // card number.
        assert!(!luhn_valid("1234567890123456"));
        assert!(!contains_card_number("1234567890123456"));
    }

    #[test]
    fn zip_plus_four_is_not_a_card_number() {
        assert!(!contains_card_number("94103-1234"));
    }

    #[test]
    fn valid_gb_iban_is_detected() {
        assert!(contains_iban("GB29 NWBK 6016 1331 9268 19"));
        assert!(contains_iban("GB29NWBK60161331926819"));
    }

    #[test]
    fn tampered_iban_is_not_detected() {
        assert!(!contains_iban("GB29 NWBK 6016 1331 9268 18"));
    }

    #[test]
    fn is_payment_shaped_value_detects_card_and_iban_and_allows_ordinary_values() {
        assert!(is_payment_shaped_value("4111 1111 1111 1111"));
        assert!(is_payment_shaped_value("GB29 NWBK 6016 1331 9268 19"));
        assert!(!is_payment_shaped_value("1234567890123456"));
        assert!(!is_payment_shaped_value("+1 555-123-4567"));
        assert!(!is_payment_shaped_value("94103-1234"));
    }
}
