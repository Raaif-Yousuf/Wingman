//! The profile's hard payment-data denylist (issue #36, AGENTS.md's "never
//! stores payment data" and the expansion plan's confirm rule 4: "the
//! profile has no card, bank or password fields").
//!
//! [`check_field`] is the single entry point [`super::Profile::validate`]
//! calls for every field, on both write and load. A violation is reported
//! as `Err(String)` -- a clear, specific reason -- and the caller is
//! responsible for never echoing the offending value in any message shown
//! to the user or written to a log (this module's own error strings never
//! contain the value either, only the field name and the kind of match).
//!
//! No em dashes in the messages (AGENTS.md rule 11): they are user-facing,
//! since a save that fails this check surfaces the message on a card.
//!
//! Deliberately dependency-free (no `regex`): every check below is a plain
//! byte/char scan, consistent with the rest of this crate's very small
//! dependency footprint.

/// Checks one profile field's value for anything payment-shaped. `field`
/// names the field for the error message only (e.g. `"notes"`,
/// `"email #2"`) -- it is never itself scanned for card/IBAN patterns
/// (a field literally named "IBAN" is not itself an IBAN), except for the
/// CVV/bank-account label checks, which look at wording, not digits, and so
/// apply to `field` too in case a future field is ever named that way.
pub fn check_field(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Ok(());
    }
    if contains_card_number(value) {
        return Err(format!(
            "profile field '{field}' looks like a card number and was rejected. Nothing was saved."
        ));
    }
    if contains_iban(value) {
        return Err(format!(
            "profile field '{field}' looks like an IBAN and was rejected. Nothing was saved."
        ));
    }
    if contains_cvv_label(field) || contains_cvv_label(value) {
        return Err(format!(
            "profile field '{field}' looks like a CVV or card verification value and was rejected. Nothing was saved."
        ));
    }
    if contains_bank_label(field) || contains_bank_label(value) {
        return Err(format!(
            "profile field '{field}' looks like a bank account or routing number and was rejected. Nothing was saved."
        ));
    }
    Ok(())
}

// -- card numbers, IBAN: value-shape checks -------------------------------
//
// #220 (closed): these used to be their own implementation here, read by
// nothing else, on the theory that only a profile write/load ever needed a
// value-shape check (a `"model"`-sourced form_fill value had no equivalent
// guarantee, and neither `actions::fill_form::merge_model_response` nor
// `executors::fill_form::evaluate_resolved_field` called anything here).
// Both now read from `crate::payment_denylist`, which owns the Luhn/IBAN
// logic canonically, the same reconciliation #215 already did for the
// label-shaped term tables just below. `luhn_valid` is exposed there as
// `pub(crate)` purely so this file's own tests can still exercise the
// checksum in isolation, the same way they did before the move.

fn contains_card_number(text: &str) -> bool {
    crate::payment_denylist::contains_card_number(text)
}

fn luhn_valid(digits: &str) -> bool {
    crate::payment_denylist::luhn_valid(digits)
}

fn contains_iban(text: &str) -> bool {
    crate::payment_denylist::contains_iban(text)
}

// -- CVV / bank-account / routing labels ------------------------------------
//
// #215 (closed): these two term lists used to be maintained here
// independently of `executors::fill_form::PAYMENT_LABEL_TERMS`'s own
// wording for the same concepts. Both now read from the one shared table in
// `crate::payment_denylist`, so a term added to cover a gap in one caller is
// automatically covered in the other.

fn contains_cvv_label(text: &str) -> bool {
    crate::payment_denylist::contains_any(text, crate::payment_denylist::CVV_TERMS)
}

fn contains_bank_label(text: &str) -> bool {
    crate::payment_denylist::contains_any(text, crate::payment_denylist::BANK_TERMS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Luhn / card numbers ---------------------------------------------

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
    fn card_number_embedded_in_prose_is_detected() {
        assert!(contains_card_number(
            "my card is 4111 1111 1111 1111 expiring soon"
        ));
    }

    #[test]
    fn short_digit_run_is_not_a_card_number() {
        assert!(!contains_card_number("12345"));
    }

    #[test]
    fn long_but_luhn_invalid_digit_run_is_not_a_card_number() {
        // 19 digits, deliberately not Luhn-valid (verified by hand: the
        // Luhn sum over these digits is 91, not a multiple of 10).
        assert!(!luhn_valid("1234567891234567891"));
        assert!(!contains_card_number("1234567891234567891"));
    }

    #[test]
    fn ordinary_phone_number_is_not_a_card_number() {
        assert!(!contains_card_number("+1 555-123-4567"));
    }

    // -- IBAN --------------------------------------------------------------

    #[test]
    fn valid_gb_iban_is_detected() {
        // A well-known IBAN example (GB29 NWBK 6016 1331 9268 19, from the
        // Wikipedia/ISO 13616 worked example), mod-97 valid.
        assert!(contains_iban("GB29 NWBK 6016 1331 9268 19"));
        assert!(contains_iban("GB29NWBK60161331926819"));
    }

    #[test]
    fn valid_de_iban_is_detected() {
        assert!(contains_iban("DE89 3704 0044 0532 0130 00"));
    }

    #[test]
    fn tampered_iban_is_not_detected() {
        assert!(!contains_iban("GB29 NWBK 6016 1331 9268 18"));
    }

    #[test]
    fn ordinary_long_identifier_is_not_an_iban() {
        // Long alphanumeric ID that does not start letter-letter-digit-digit.
        assert!(!contains_iban("ORDER-REF-1234567890123456"));
    }

    #[test]
    fn short_string_is_not_an_iban() {
        assert!(!contains_iban("GB29NWBK6"));
    }

    // -- CVV label ----------------------------------------------------------

    #[test]
    fn cvv_label_variants_are_detected() {
        assert!(contains_cvv_label("CVV: 123"));
        assert!(contains_cvv_label("cvc2 code"));
        assert!(contains_cvv_label("Security Code"));
        assert!(contains_cvv_label("card verification value"));
    }

    #[test]
    fn ordinary_text_is_not_a_cvv_label() {
        assert!(!contains_cvv_label(
            "Just a regular note about dinner plans."
        ));
    }

    // -- bank/routing label --------------------------------------------------

    #[test]
    fn bank_label_variants_are_detected() {
        assert!(contains_bank_label("Account Number: 12345678"));
        assert!(contains_bank_label("Routing Number 021000021"));
        assert!(contains_bank_label("Sort code 12-34-56"));
        assert!(contains_bank_label("SWIFT code ABCDEF12"));
    }

    #[test]
    fn ordinary_text_is_not_a_bank_label() {
        assert!(!contains_bank_label(
            "Meet at the account desk for the conference badge."
        ));
    }

    // -- check_field: the public entry point ---------------------------------

    #[test]
    fn check_field_accepts_ordinary_values() {
        assert!(check_field("full_name", "Ada Lovelace").is_ok());
        assert!(check_field("email #1", "ada@example.com").is_ok());
        assert!(check_field("notes", "Prefers async standups.").is_ok());
        assert!(check_field("phone #1", "+1 555-123-4567").is_ok());
    }

    #[test]
    fn check_field_accepts_empty_values() {
        assert!(check_field("website", "").is_ok());
        assert!(check_field("notes", "   ").is_ok());
    }

    #[test]
    fn check_field_rejects_card_number_and_names_the_field() {
        let err = check_field("notes", "card: 4111 1111 1111 1111").unwrap_err();
        assert!(err.contains("notes"));
        assert!(err.contains("card number"));
        assert!(
            !err.contains("4111"),
            "error must never echo the value: {err}"
        );
    }

    #[test]
    fn check_field_rejects_iban_and_names_the_field() {
        let err = check_field("notes", "GB29 NWBK 6016 1331 9268 19").unwrap_err();
        assert!(err.contains("notes"));
        assert!(err.contains("IBAN"));
        assert!(
            !err.contains("NWBK"),
            "error must never echo the value: {err}"
        );
    }

    #[test]
    fn check_field_rejects_cvv_label_and_names_the_field() {
        let err = check_field("notes", "CVV: 123").unwrap_err();
        assert!(err.contains("notes"));
        assert!(err.contains("CVV"));
        assert!(
            !err.contains("123"),
            "error must never echo the value: {err}"
        );
    }

    #[test]
    fn check_field_rejects_bank_label_and_names_the_field() {
        let err = check_field("notes", "Account Number: 555444333").unwrap_err();
        assert!(err.contains("notes"));
        assert!(err.contains("bank account"));
        assert!(
            !err.contains("555444333"),
            "error must never echo the value: {err}"
        );
    }

    #[test]
    fn check_field_error_messages_have_no_em_dash() {
        let err = check_field("notes", "CVV: 123").unwrap_err();
        assert!(
            !err.contains('\u{2014}'),
            "no em dashes in user-facing strings: {err}"
        );
    }
}
