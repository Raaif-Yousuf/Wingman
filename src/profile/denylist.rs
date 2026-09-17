//! The profile's hard payment-data denylist (issue #36, CLAUDE.md's "never
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
//! No em dashes in the messages (CLAUDE.md rule 11): they are user-facing,
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

// -- card numbers: Luhn-valid 12-19 digit runs, separators allowed --------

/// True if `text` contains a run of digits, optionally separated by spaces
/// or hyphens, whose digit count is 12 to 19 (the range real card numbers
/// fall in) and which passes the Luhn checksum.
fn contains_card_number(text: &str) -> bool {
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

/// Standard Luhn checksum over an all-digit string.
fn luhn_valid(digits: &str) -> bool {
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

// -- IBAN: mod-97 valid ----------------------------------------------------

/// True if `text` contains a substring shaped like a real IBAN (2 letters +
/// 2 check digits + 11-30 more alphanumerics, spaces allowed between every
/// 4 characters as IBANs are conventionally printed) that passes the
/// ISO 7064 MOD97-10 check.
fn contains_iban(text: &str) -> bool {
    for run in alnum_runs_with_spaces(text) {
        let len = run.len();
        if (15..=34).contains(&len) && iban_mod97_valid(&run) {
            return true;
        }
    }
    false
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

// -- CVV / bank-account / routing labels ------------------------------------

const CVV_LABELS: &[&str] = &[
    "cvv2",
    "cvc2",
    "cvv",
    "cvc",
    "security code",
    "card verification",
];

const BANK_LABELS: &[&str] = &[
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

fn contains_cvv_label(text: &str) -> bool {
    let lower = text.to_lowercase();
    CVV_LABELS.iter().any(|needle| lower.contains(needle))
}

fn contains_bank_label(text: &str) -> bool {
    let lower = text.to_lowercase();
    BANK_LABELS.iter().any(|needle| lower.contains(needle))
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
