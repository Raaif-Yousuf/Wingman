//! Label matching: maps a form field's visible label (e.g. "E-mail
//! address", "Phone", "Postcode/ZIP") to a [`FieldKind`] this profile
//! stores a value for. Scaffolding for the "Fill this form" action (issue
//! #40, expansion plan §6), which is not implemented yet -- this module
//! only provides the pure, tested mapping it will need. No UI, no executor,
//! no Win32 here; see #50 for the profile settings page and #40 for the
//! action itself.
//!
//! Pure by design (AGENTS.md rule 8: pure logic is unit-tested) so it can
//! be exercised without a UIA tree or a live form.

/// One profile field a form label can be matched to. Deliberately flatter
/// than [`super::Profile`]'s own shape (e.g. `AddressLine`/`City`/... are
/// separate variants even though they all live under `Profile::address`):
/// a form asks for one control at a time, and `fill_form` (#40) will read
/// off whichever `Profile` sub-value each variant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldKind {
    FullName,
    PreferredName,
    Email,
    Phone,
    AddressLine,
    City,
    Region,
    Postcode,
    Country,
    Organisation,
    JobTitle,
    DateOfBirth,
    Website,
}

/// `(kind, synonyms)`. Checked in order, so a more specific synonym (e.g.
/// `"preferred name"`) must be listed before a more general one that could
/// also match it (e.g. plain `"name"`); [`match_label`] returns the first
/// kind with any synonym contained in the normalized label.
const SYNONYMS: &[(FieldKind, &[&str])] = &[
    (
        FieldKind::PreferredName,
        &[
            "preferred name",
            "nickname",
            "display name",
            "goes by",
            "known as",
        ],
    ),
    (
        FieldKind::FullName,
        &["full name", "your name", "legal name", "fullname", "name"],
    ),
    (
        FieldKind::Email,
        &["email", "e mail", "email address", "e mail address"],
    ),
    (
        FieldKind::Phone,
        &[
            "phone",
            "telephone",
            "mobile",
            "cell",
            "phone number",
            "contact number",
        ],
    ),
    (
        FieldKind::Postcode,
        &["postcode", "postal code", "zip", "zip code"],
    ),
    (FieldKind::Country, &["country", "nation"]),
    (
        FieldKind::Region,
        &["state", "province", "region", "county"],
    ),
    (FieldKind::City, &["city", "town"]),
    (
        FieldKind::AddressLine,
        &["address line", "street address", "address", "street"],
    ),
    (
        FieldKind::Organisation,
        &["organisation", "organization", "company", "employer"],
    ),
    (
        FieldKind::JobTitle,
        &["job title", "job role", "title", "position", "role"],
    ),
    (
        FieldKind::DateOfBirth,
        &["date of birth", "dob", "birth date", "birthday"],
    ),
    (
        FieldKind::Website,
        &["website", "web site", "homepage", "url"],
    ),
];

/// Normalizes a form label for matching: lowercased, with any character
/// that is not an ASCII letter or digit collapsed to a single space (so
/// `"E-mail address"` becomes `"e mail address"` and `"Postcode/ZIP"`
/// becomes `"postcode zip"`), and outer whitespace trimmed. Runs of spaces
/// from adjacent separators (`"Postcode//ZIP"`) are collapsed to one.
fn normalize(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut last_was_space = true; // suppresses a leading space
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Maps a form label to the [`FieldKind`] it most likely asks for, or
/// `None` if nothing in the synonym table matches. Matching is substring
/// based on the normalized label, in [`SYNONYMS`]'s declared order, so
/// `"E-mail address"` normalizes to `"e mail address"`, which contains the
/// `Email` synonym `"e mail address"` and returns `Some(FieldKind::Email)`.
pub fn match_label(label: &str) -> Option<FieldKind> {
    let normalized = normalize(label);
    if normalized.is_empty() {
        return None;
    }
    for (kind, synonyms) in SYNONYMS {
        if synonyms.iter().any(|syn| normalized.contains(syn)) {
            return Some(*kind);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_punctuation_and_lowercases() {
        assert_eq!(normalize("E-mail address"), "e mail address");
        assert_eq!(normalize("Postcode/ZIP"), "postcode zip");
        assert_eq!(normalize("  Phone  "), "phone");
        assert_eq!(normalize("Postcode//ZIP"), "postcode zip");
    }

    #[test]
    fn normalize_of_empty_label_is_empty() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
    }

    // -- the task's own worked examples --------------------------------

    #[test]
    fn matches_email_address_label() {
        assert_eq!(match_label("E-mail address"), Some(FieldKind::Email));
    }

    #[test]
    fn matches_bare_phone_label() {
        assert_eq!(match_label("Phone"), Some(FieldKind::Phone));
    }

    #[test]
    fn matches_postcode_zip_label() {
        assert_eq!(match_label("Postcode/ZIP"), Some(FieldKind::Postcode));
    }

    // -- a representative synonym per kind -------------------------------

    #[test]
    fn matches_full_name_synonyms() {
        assert_eq!(match_label("Full Name"), Some(FieldKind::FullName));
        assert_eq!(match_label("Your name"), Some(FieldKind::FullName));
        assert_eq!(match_label("Legal Name"), Some(FieldKind::FullName));
    }

    #[test]
    fn matches_preferred_name_before_full_name() {
        // "preferred name" contains neither of these being confused for
        // plain "name" -- PreferredName must win since it is more specific
        // and listed first.
        assert_eq!(
            match_label("Preferred name"),
            Some(FieldKind::PreferredName)
        );
        assert_eq!(match_label("Nickname"), Some(FieldKind::PreferredName));
    }

    #[test]
    fn matches_address_related_labels() {
        assert_eq!(match_label("Street Address"), Some(FieldKind::AddressLine));
        assert_eq!(match_label("City"), Some(FieldKind::City));
        assert_eq!(match_label("State/Province"), Some(FieldKind::Region));
        assert_eq!(match_label("Country"), Some(FieldKind::Country));
    }

    #[test]
    fn matches_organisation_and_job_title() {
        assert_eq!(match_label("Company"), Some(FieldKind::Organisation));
        assert_eq!(match_label("Job Title"), Some(FieldKind::JobTitle));
    }

    #[test]
    fn matches_date_of_birth_and_website() {
        assert_eq!(match_label("Date of Birth"), Some(FieldKind::DateOfBirth));
        assert_eq!(match_label("DOB"), Some(FieldKind::DateOfBirth));
        assert_eq!(match_label("Website"), Some(FieldKind::Website));
    }

    // -- no match ----------------------------------------------------------

    #[test]
    fn unrecognized_label_is_none() {
        assert_eq!(match_label("Favorite color"), None);
        assert_eq!(match_label(""), None);
    }

    #[test]
    fn is_case_and_punctuation_insensitive() {
        assert_eq!(match_label("EMAIL"), Some(FieldKind::Email));
        assert_eq!(match_label("e-mail"), Some(FieldKind::Email));
        assert_eq!(match_label("E.Mail"), Some(FieldKind::Email));
    }
}
