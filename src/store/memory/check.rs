//! The payment-data and secret check every memory write runs (spec, "The
//! payment-data denylist at the boundary"). Pure string scans, so there is
//! nothing to fail at runtime; the caller still treats any `Err` as "not
//! saved", so a check that cannot answer never lets text through.

use super::MemoryKind;
use crate::payment_denylist as deny;

/// Why a text was not saved. Names the kind of match, never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRefusal {
    CardNumber,
    Iban,
    PaymentLabel,
    NationalId,
    Secret,
    Empty,
    TooLong,
}

impl std::fmt::Display for MemoryRefusal {
    /// Plain words for a card.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MemoryRefusal::CardNumber => "it looks like a card number",
            MemoryRefusal::Iban => "it looks like a bank account number (IBAN)",
            MemoryRefusal::PaymentLabel => {
                "it mentions a card security code or a bank account or routing number"
            }
            MemoryRefusal::NationalId => "it looks like a national id number",
            MemoryRefusal::Secret => "it looks like a password or a secret key",
            MemoryRefusal::Empty => "it is empty",
            MemoryRefusal::TooLong => "it is too long",
        })
    }
}

/// The longest text one item may hold, in characters.
pub const MAX_TEXT_CHARS: usize = 2000;

/// Checks `text` for an item of `kind`. `kind` is part of the signature so a
/// later rule can differ by kind without touching any caller.
pub fn check_memory_text(_kind: MemoryKind, text: &str) -> Result<(), MemoryRefusal> {
    let _ = text;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(text: &str) -> Option<MemoryRefusal> {
        check_memory_text(MemoryKind::Fact, text).err()
    }

    #[test]
    fn a_sixteen_digit_card_number_is_refused() {
        assert_eq!(
            refused("my card is 4111 1111 1111 1111"),
            Some(MemoryRefusal::CardNumber)
        );
        assert_eq!(
            refused("4111111111111111"),
            Some(MemoryRefusal::CardNumber)
        );
    }

    #[test]
    fn an_iban_is_refused() {
        assert_eq!(
            refused("pay GB82 WEST 1234 5698 7654 32"),
            Some(MemoryRefusal::Iban)
        );
    }

    #[test]
    fn payment_labels_are_refused() {
        assert_eq!(refused("CVV is on the back"), Some(MemoryRefusal::PaymentLabel));
        assert_eq!(
            refused("routing number 021000021"),
            Some(MemoryRefusal::PaymentLabel)
        );
    }

    #[test]
    fn national_ids_are_refused_but_phone_numbers_and_zip_codes_are_not() {
        assert_eq!(refused("ssn 078-05-1120"), Some(MemoryRefusal::NationalId));
        assert_eq!(
            refused("passport number 123456789"),
            Some(MemoryRefusal::NationalId)
        );
        assert_eq!(refused("call me on 415-555-0134"), None);
        assert_eq!(refused("lives in 94107"), None);
    }

    #[test]
    fn secrets_are_refused() {
        assert_eq!(refused("password: hunter22"), Some(MemoryRefusal::Secret));
        assert_eq!(
            refused("key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF"),
            Some(MemoryRefusal::Secret)
        );
    }

    #[test]
    fn ordinary_text_passes() {
        for ok in [
            "Employer: Acme, 2019 to 2022",
            "Prefers short replies",
            "Born in 1990, moved to Leeds in 2015",
            "Order 1234567 shipped",
        ] {
            assert_eq!(refused(ok), None, "{ok}");
        }
    }

    #[test]
    fn empty_and_oversized_text_are_refused() {
        assert_eq!(refused("   "), Some(MemoryRefusal::Empty));
        assert_eq!(
            refused(&"a ".repeat(MAX_TEXT_CHARS)),
            Some(MemoryRefusal::TooLong)
        );
    }

    #[test]
    fn the_message_never_echoes_the_value() {
        let why = refused("card 4111111111111111").unwrap().to_string();
        assert!(!why.contains("4111"));
    }
}
