//! The connector registry (#35): the one place a connector name (today only
//! set by `calendar_add`'s fixed `"ics"` choice -- see the connector design
//! doc's `calendar_add executor` section) turns into a real
//! [`super::Connector`]. A `match`, not a `HashMap`/`once_cell` lookup
//! table, same reasoning as `executors::registry::resolve`: with one entry
//! a data structure buys nothing a `match` doesn't already give for free.

use super::{Connector, IcsConnector};

/// Resolves a connector by name, or fails with the exact text an unknown
/// connector setting would surface on an error card (AGENTS.md rule 7: a
/// load error, not a panic; rule 11: no em dash).
pub fn resolve(name: &str) -> anyhow::Result<Box<dyn Connector>> {
    match name {
        "ics" => Ok(Box::new(IcsConnector::default())),
        _ => anyhow::bail!("No connector named \"{name}\". Check the action's connector setting."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_ics_by_name() {
        let connector = resolve("ics").expect("\"ics\" is a built-in connector");
        assert_eq!(connector.id(), "ics");
    }

    #[test]
    fn unknown_name_is_an_error_naming_it_with_no_em_dash() {
        let err = resolve("does-not-exist")
            .err()
            .expect("an unknown connector name must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains("does-not-exist"),
            "error should name the unknown connector: {msg}"
        );
        assert!(
            !msg.contains('\u{2014}'),
            "no em dashes in card-facing text (rule 11): {msg}"
        );
    }
}
