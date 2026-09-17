//! The one shared table of payment-shaped *label* terms (issue #215):
//! before this file, `executors::fill_form::is_payment_label` (a proposal
//! field's label, checked before ever writing it) and
//! `profile::denylist::contains_cvv_label`/`contains_bank_label` (a profile
//! field's name or value, checked on every save and load) each maintained
//! their own separately-worded term list for the same underlying concept
//! (CLAUDE.md rule: never store or fill payment data). Filed as #215 the
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
//! This module only reconciles *label*-shaped term matching. The
//! *value*-shape checks (Luhn-valid digit runs, mod-97-valid IBANs) stay in
//! `profile::denylist`, since `executors::fill_form` never needed them (see
//! that module's own doc comment: it refuses by label, before a value is
//! ever written, because the profile has no payment fields to source one
//! from in the first place).

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
}
