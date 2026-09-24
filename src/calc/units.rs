//! Issue #115: unit conversion, `"<number> <unit> in|to <unit>"`, no model
//! involved. Length, mass, volume, temperature (affine, not a linear
//! factor), time, data (decimal KB vs. binary KiB), speed, area, energy
//! (issue #316) and pressure (issue #316).
//!
//! Every non-temperature dimension converts through a fixed base unit (a
//! plain multiplicative factor); temperature is the one dimension that needs
//! an affine (scale AND offset) conversion, so every unit stores a pair of
//! `f64 -> f64` functions (`to_base`/`from_base`) rather than a single
//! factor -- a linear unit's pair is just `|x| x * k` / `|x| x / k`, so this
//! one shape covers both without a special case at the call site.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Length,
    Mass,
    Volume,
    Temperature,
    Time,
    Data,
    Speed,
    Area,
    Energy,
    Pressure,
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Dimension::Length => "length",
            Dimension::Mass => "mass",
            Dimension::Volume => "volume",
            Dimension::Temperature => "temperature",
            Dimension::Time => "time",
            Dimension::Data => "data",
            Dimension::Speed => "speed",
            Dimension::Area => "area",
            Dimension::Energy => "energy",
            Dimension::Pressure => "pressure",
        };
        write!(f, "{s}")
    }
}

struct UnitDef {
    /// Every recognized spelling, lowercase, checked first-match; the first
    /// alias is the canonical display name.
    aliases: &'static [&'static str],
    dimension: Dimension,
    to_base: fn(f64) -> f64,
    from_base: fn(f64) -> f64,
}

/// Builds a linear (non-affine) unit entry: `to_base = |x| x * factor`,
/// `from_base = |x| x / factor`. Temperature is the one dimension below that
/// doesn't use this macro, because it needs a real offset, not just a scale
/// (see the module doc comment).
macro_rules! linear_unit {
    ($aliases:expr, $dimension:expr, $factor:expr) => {
        UnitDef {
            aliases: $aliases,
            dimension: $dimension,
            to_base: |x: f64| x * $factor,
            from_base: |x: f64| x / $factor,
        }
    };
}

fn units_table() -> &'static [UnitDef] {
    use Dimension::*;
    static TABLE: std::sync::OnceLock<Vec<UnitDef>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        vec![
            // -- Length (base: meter) --------------------------------------
            linear_unit!(
                &[
                    "mm",
                    "millimeter",
                    "millimeters",
                    "millimetre",
                    "millimetres"
                ],
                Length,
                0.001
            ),
            linear_unit!(
                &[
                    "cm",
                    "centimeter",
                    "centimeters",
                    "centimetre",
                    "centimetres"
                ],
                Length,
                0.01
            ),
            linear_unit!(&["m", "meter", "meters", "metre", "metres"], Length, 1.0),
            linear_unit!(
                &["km", "kilometer", "kilometers", "kilometre", "kilometres"],
                Length,
                1000.0
            ),
            linear_unit!(&["in", "inch", "inches"], Length, 0.0254),
            linear_unit!(&["ft", "foot", "feet"], Length, 0.3048),
            linear_unit!(&["yd", "yard", "yards"], Length, 0.9144),
            linear_unit!(&["mi", "mile", "miles"], Length, 1609.344),
            // -- Mass (base: kilogram) ---------------------------------------
            linear_unit!(&["mg", "milligram", "milligrams"], Mass, 1e-6),
            linear_unit!(&["g", "gram", "grams"], Mass, 0.001),
            linear_unit!(&["kg", "kilogram", "kilograms"], Mass, 1.0),
            linear_unit!(&["lb", "lbs", "pound", "pounds"], Mass, 0.45359237),
            linear_unit!(&["oz", "ounce", "ounces"], Mass, 0.028349523125),
            linear_unit!(
                &["ton", "tonne", "tonnes", "metric ton", "metric tons"],
                Mass,
                1000.0
            ),
            // -- Volume (base: liter) -----------------------------------------
            linear_unit!(
                &[
                    "ml",
                    "milliliter",
                    "milliliters",
                    "millilitre",
                    "millilitres"
                ],
                Volume,
                0.001
            ),
            linear_unit!(&["l", "liter", "liters", "litre", "litres"], Volume, 1.0),
            linear_unit!(&["gal", "gallon", "gallons"], Volume, 3.785411784),
            linear_unit!(&["qt", "quart", "quarts"], Volume, 0.946352946),
            linear_unit!(&["pt", "pint", "pints"], Volume, 0.473176473),
            linear_unit!(&["cup", "cups"], Volume, 0.2365882365),
            linear_unit!(
                &["floz", "fl oz", "fluid ounce", "fluid ounces"],
                Volume,
                0.0295735295625
            ),
            // -- Time (base: second) -------------------------------------------
            linear_unit!(&["ms", "millisecond", "milliseconds"], Time, 0.001),
            linear_unit!(&["s", "sec", "secs", "second", "seconds"], Time, 1.0),
            linear_unit!(&["min", "mins", "minute", "minutes"], Time, 60.0),
            linear_unit!(&["h", "hr", "hrs", "hour", "hours"], Time, 3600.0),
            linear_unit!(&["day", "days"], Time, 86400.0),
            // -- Data (base: byte; KB is decimal, KiB is binary) ---------------
            linear_unit!(&["b", "byte", "bytes"], Data, 1.0),
            linear_unit!(&["kb", "kilobyte", "kilobytes"], Data, 1e3),
            linear_unit!(&["mb", "megabyte", "megabytes"], Data, 1e6),
            linear_unit!(&["gb", "gigabyte", "gigabytes"], Data, 1e9),
            linear_unit!(&["tb", "terabyte", "terabytes"], Data, 1e12),
            linear_unit!(&["kib", "kibibyte", "kibibytes"], Data, 1024.0),
            linear_unit!(&["mib", "mebibyte", "mebibytes"], Data, 1024.0 * 1024.0),
            linear_unit!(
                &["gib", "gibibyte", "gibibytes"],
                Data,
                1024.0 * 1024.0 * 1024.0
            ),
            linear_unit!(
                &["tib", "tebibyte", "tebibytes"],
                Data,
                1024.0 * 1024.0 * 1024.0 * 1024.0
            ),
            // -- Speed (base: meter/second) -------------------------------------
            linear_unit!(&["mps", "m/s"], Speed, 1.0),
            linear_unit!(&["kmh", "km/h", "kph"], Speed, 1000.0 / 3600.0),
            linear_unit!(&["mph"], Speed, 0.44704),
            linear_unit!(&["kn", "knot", "knots"], Speed, 0.5144444444444445),
            // -- Area (base: square meter) --------------------------------------
            linear_unit!(&["sqm", "m2", "square meter", "square meters"], Area, 1.0),
            linear_unit!(
                &["sqkm", "km2", "square kilometer", "square kilometers"],
                Area,
                1_000_000.0
            ),
            linear_unit!(
                &["sqft", "ft2", "square foot", "square feet"],
                Area,
                0.09290304
            ),
            linear_unit!(
                &["sqmi", "mi2", "square mile", "square miles"],
                Area,
                2_589_988.110336
            ),
            linear_unit!(&["acre", "acres"], Area, 4046.8564224),
            linear_unit!(&["hectare", "hectares", "ha"], Area, 10_000.0),
            // -- Energy (base: joule; factors NIST SP 811) -----------------------
            linear_unit!(&["j", "joule", "joules"], Energy, 1.0),
            linear_unit!(&["kj", "kilojoule", "kilojoules"], Energy, 1000.0),
            // Thermochemical calorie, NIST SP 811: 1 cal = 4.184 J.
            linear_unit!(&["cal", "calorie", "calories"], Energy, 4.184),
            // Food "Calorie" is a kilocalorie: 1 kcal = 4184 J.
            linear_unit!(&["kcal", "kilocalorie", "kilocalories"], Energy, 4184.0),
            linear_unit!(&["wh", "watt hour", "watt hours"], Energy, 3600.0),
            linear_unit!(
                &["kwh", "kilowatt hour", "kilowatt hours"],
                Energy,
                3_600_000.0
            ),
            // British thermal unit (IT), NIST SP 811: 1 BTU = 1055.05585262 J.
            linear_unit!(&["btu"], Energy, 1055.05585262),
            // -- Pressure (base: pascal; factors NIST SP 811) --------------------
            linear_unit!(&["pa", "pascal", "pascals"], Pressure, 1.0),
            linear_unit!(&["kpa", "kilopascal", "kilopascals"], Pressure, 1000.0),
            linear_unit!(&["bar", "bars"], Pressure, 100_000.0),
            // 1 psi = 6894.757293168... Pa (NIST SP 811).
            linear_unit!(&["psi"], Pressure, 6894.757293168361),
            linear_unit!(&["atm", "atmosphere", "atmospheres"], Pressure, 101_325.0),
            // 1 mmHg = 133.322387415 Pa (NIST SP 811, conventional mmHg).
            linear_unit!(&["mmhg"], Pressure, 133.322387415),
            // -- Temperature (base: kelvin, affine) -----------------------------
            UnitDef {
                aliases: &["c", "celsius"],
                dimension: Temperature,
                to_base: |c| c + 273.15,
                from_base: |k| k - 273.15,
            },
            UnitDef {
                aliases: &["f", "fahrenheit"],
                dimension: Temperature,
                to_base: |f| (f - 32.0) * 5.0 / 9.0 + 273.15,
                from_base: |k| (k - 273.15) * 9.0 / 5.0 + 32.0,
            },
            UnitDef {
                aliases: &["k", "kelvin"],
                dimension: Temperature,
                to_base: |k| k,
                from_base: |k| k,
            },
        ]
    })
}

fn find_unit(name: &str) -> Option<&'static UnitDef> {
    let key = name.trim().to_ascii_lowercase();
    units_table()
        .iter()
        .find(|u| u.aliases.iter().any(|&a| a == key))
}

/// Canonical (first-alias) display name for whatever unit `name` resolves
/// to, or `name` itself, unchanged, if it isn't recognized -- used only to
/// report an unknown unit back with the user's own spelling.
fn canonical_name(name: &str) -> String {
    find_unit(name)
        .map(|u| u.aliases[0].to_string())
        .unwrap_or_else(|| name.to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnitError {
    UnknownUnit(String),
    IncompatibleDimensions {
        from_unit: String,
        from_dim: Dimension,
        to_unit: String,
        to_dim: Dimension,
    },
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitError::UnknownUnit(name) => write!(f, "'{name}' isn't a unit I know."),
            UnitError::IncompatibleDimensions {
                from_unit,
                from_dim,
                to_unit,
                to_dim,
            } => write!(
                f,
                "Can't convert {from_unit} ({from_dim}) to {to_unit} ({to_dim})."
            ),
        }
    }
}

/// Converts `value` from `from` to `to`. Both are matched case-insensitively
/// against [`units_table`]'s aliases.
pub fn convert(value: f64, from: &str, to: &str) -> Result<f64, UnitError> {
    let from_def = find_unit(from).ok_or_else(|| UnitError::UnknownUnit(from.to_string()))?;
    let to_def = find_unit(to).ok_or_else(|| UnitError::UnknownUnit(to.to_string()))?;
    if from_def.dimension != to_def.dimension {
        return Err(UnitError::IncompatibleDimensions {
            from_unit: canonical_name(from),
            from_dim: from_def.dimension,
            to_unit: canonical_name(to),
            to_dim: to_def.dimension,
        });
    }
    let base = (from_def.to_base)(value);
    Ok((to_def.from_base)(base))
}

/// One `"<number> <unit> in|to <unit>"` query, already split apart -- pure
/// tokenizing, no unit lookup (that's [`convert`]'s job, so an unrecognized
/// unit is reported by `convert`, not silently treated as "not a conversion
/// query at all").
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedConversion {
    pub value: f64,
    pub from: String,
    pub to: String,
}

/// Parses `"<number> <unit...> (in|to) <unit...>"`. Returns `None` when the
/// input doesn't have that shape at all (no leading number, or no "in"/"to"
/// separator token) -- the caller ([`super::evaluate`]) then falls back to
/// treating the input as an arithmetic expression instead. Once this DOES
/// return `Some`, an unrecognized unit is a [`UnitError`] from [`convert`],
/// not a silent fallback -- "in"/"to" essentially never appears inside a
/// bare arithmetic expression, so treating its presence as a firm commitment
/// to "this is a conversion query" gives a much clearer error than trying to
/// parse `"3.5 bananas in km"` as arithmetic.
pub fn parse_conversion_query(input: &str) -> Option<ParsedConversion> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.len() < 3 {
        return None;
    }
    let sep_idx = tokens
        .iter()
        .position(|t| t.eq_ignore_ascii_case("in") || t.eq_ignore_ascii_case("to"))?;
    if sep_idx == 0 || sep_idx == tokens.len() - 1 {
        return None;
    }
    let value = parse_leading_number(tokens[0])?;
    let from = tokens[1..sep_idx].join(" ");
    let to = tokens[sep_idx + 1..].join(" ");
    if from.is_empty() || to.is_empty() {
        return None;
    }
    Some(ParsedConversion { value, from, to })
}

/// Parses one whitespace-free numeric token: decimals, thousands separators
/// and scientific notation, the same literal shape [`super::expr`]'s
/// tokenizer accepts for a single number (deliberately reimplemented small
/// and standalone here rather than shared, since this module has no
/// dependency on the expression parser's `Token`/`Parser` machinery and
/// pulling those in would be a bigger coupling than repeating ~10 lines of
/// digit-scanning).
fn parse_leading_number(token: &str) -> Option<f64> {
    let cleaned: String = token.chars().filter(|&c| c != ',').collect();
    cleaned.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6 * b.abs().max(1.0)
    }

    // -- table lookups / conversions -----------------------------------------

    #[test]
    fn table_of_conversions() {
        let cases: &[(f64, &str, &str, f64)] = &[
            (3.5, "mi", "km", 5.632704),
            (1.0, "km", "m", 1000.0),
            (1000.0, "m", "km", 1.0),
            (1.0, "kg", "lb", 2.2046226218487757),
            (1.0, "lb", "oz", 16.0),
            (1.0, "gal", "l", 3.785411784),
            (0.0, "c", "f", 32.0),
            (100.0, "c", "f", 212.0),
            (32.0, "f", "c", 0.0),
            (0.0, "c", "k", 273.15),
            (0.0, "k", "c", -273.15),
            (60.0, "min", "s", 3600.0),
            (1.0, "h", "min", 60.0),
            (1.0, "kb", "b", 1000.0),
            (1.0, "kib", "b", 1024.0),
            (1.0, "mib", "kib", 1024.0),
            (100.0, "kmh", "mph", 62.13711922),
            (1.0, "sqkm", "sqm", 1_000_000.0),
            (1.0, "acre", "sqm", 4046.8564224),
            // -- Energy (from issue #316) -----------------------------------
            (1.0, "kcal", "j", 4184.0),
            (1.0, "kwh", "j", 3_600_000.0),
            (1.0, "cal", "j", 4.184),
            (1.0, "j", "cal", 1.0 / 4.184),
            (1.0, "kj", "j", 1000.0),
            (1.0, "wh", "j", 3600.0),
            (1.0, "kwh", "kcal", 3_600_000.0 / 4184.0),
            (1.0, "btu", "j", 1055.05585262),
            // -- Pressure (from issue #316) ----------------------------------
            (1.0, "bar", "pa", 100_000.0),
            (1.0, "atm", "pa", 101325.0),
            (1.0, "kpa", "pa", 1000.0),
            (1.0, "mmhg", "pa", 133.322387415),
        ];
        for (value, from, to, expected) in cases {
            let got = convert(*value, from, to)
                .unwrap_or_else(|e| panic!("{value} {from} -> {to} failed: {e}"));
            assert!(
                close(got, *expected),
                "{value} {from} -> {to}: expected {expected}, got {got}"
            );
        }
    }

    #[test]
    fn energy_and_pressure_brief_examples() {
        // 1 kcal is 4184 J.
        assert!(close(convert(1.0, "kcal", "j").unwrap(), 4184.0));
        // 1 kWh is 3.6e6 J.
        assert!(close(convert(1.0, "kwh", "j").unwrap(), 3.6e6));
        // 1 bar is 14.5038 psi (to 4 decimal places).
        let bar_to_psi = convert(1.0, "bar", "psi").unwrap();
        assert_eq!((bar_to_psi * 10_000.0).round() / 10_000.0, 14.5038);
        // 1 atm is 101325 Pa.
        assert!(close(convert(1.0, "atm", "pa").unwrap(), 101_325.0));
        // 5 kg in psi is a clear error, not a number.
        let err = convert(5.0, "kg", "psi").unwrap_err();
        assert!(matches!(err, UnitError::IncompatibleDimensions { .. }));
    }

    #[test]
    fn kcal_and_kwh_round_trip() {
        let out = convert(10.0, "kcal", "kwh").unwrap();
        let back = convert(out, "kwh", "kcal").unwrap();
        assert!(close(back, 10.0), "kcal<->kwh round trip: got {back}");
    }

    #[test]
    fn psi_and_bar_round_trip() {
        let out = convert(10.0, "psi", "bar").unwrap();
        let back = convert(out, "bar", "psi").unwrap();
        assert!(close(back, 10.0), "psi<->bar round trip: got {back}");
    }

    #[test]
    fn energy_and_pressure_are_separate_dimensions() {
        let err = convert(1.0, "psi", "kcal").unwrap_err();
        assert!(matches!(err, UnitError::IncompatibleDimensions { .. }));
        let err = convert(1.0, "kcal", "psi").unwrap_err();
        assert!(matches!(err, UnitError::IncompatibleDimensions { .. }));
    }

    #[test]
    fn cal_and_kcal_do_not_collide() {
        // "kcal" must resolve as one unit, not "k" (kelvin) + "cal".
        assert!(find_unit("kcal").is_some());
        assert_eq!(find_unit("kcal").unwrap().dimension, Dimension::Energy);
        assert_eq!(find_unit("cal").unwrap().dimension, Dimension::Energy);
        // The two units convert differently: 1 kcal != 1 cal in joules.
        let kcal_j = convert(1.0, "kcal", "j").unwrap();
        let cal_j = convert(1.0, "cal", "j").unwrap();
        assert!((kcal_j - cal_j).abs() > 1.0);
    }

    #[test]
    fn bar_does_not_collide_with_other_units() {
        assert_eq!(find_unit("bar").unwrap().dimension, Dimension::Pressure);
        // "bar" is not accidentally an alias of any other existing unit.
        for u in units_table() {
            if u.dimension != Dimension::Pressure {
                assert!(!u.aliases.contains(&"bar"));
            }
        }
    }

    #[test]
    fn round_trip_is_the_identity() {
        for (from, to) in [("mi", "km"), ("lb", "kg"), ("f", "c"), ("gal", "l")] {
            let out = convert(10.0, from, to).unwrap();
            let back = convert(out, to, from).unwrap();
            assert!(close(back, 10.0), "{from}<->{to} round trip: got {back}");
        }
    }

    #[test]
    fn unknown_unit_is_a_clear_error() {
        let err = convert(1.0, "bananas", "km").unwrap_err();
        assert_eq!(err, UnitError::UnknownUnit("bananas".to_string()));
        assert!(err.to_string().contains("bananas"));

        let err = convert(1.0, "km", "bananas").unwrap_err();
        assert_eq!(err, UnitError::UnknownUnit("bananas".to_string()));
    }

    #[test]
    fn incompatible_dimensions_is_a_clear_error() {
        let err = convert(1.0, "km", "kg").unwrap_err();
        assert!(matches!(err, UnitError::IncompatibleDimensions { .. }));
        assert!(err.to_string().contains("length"));
        assert!(err.to_string().contains("mass"));
    }

    #[test]
    fn case_insensitive_unit_names() {
        assert!(convert(1.0, "KM", "M").is_ok());
        assert!(convert(1.0, "Mi", "Km").is_ok());
    }

    // -- parse_conversion_query -----------------------------------------------

    #[test]
    fn parses_the_brief_examples() {
        let parsed = parse_conversion_query("3.5 mi in km").unwrap();
        assert_eq!(parsed.value, 3.5);
        assert_eq!(parsed.from, "mi");
        assert_eq!(parsed.to, "km");

        let parsed = parse_conversion_query("100 kg to lb").unwrap();
        assert_eq!(parsed.value, 100.0);
        assert_eq!(parsed.from, "kg");
        assert_eq!(parsed.to, "lb");
    }

    #[test]
    fn accepts_to_case_insensitively() {
        let parsed = parse_conversion_query("1 mi TO km").unwrap();
        assert_eq!(parsed.to, "km");
        let parsed = parse_conversion_query("1 mi In km").unwrap();
        assert_eq!(parsed.to, "km");
    }

    #[test]
    fn multi_word_unit_names_are_joined() {
        let parsed = parse_conversion_query("1 fl oz in ml").unwrap();
        assert_eq!(parsed.from, "fl oz");
    }

    #[test]
    fn thousands_separators_in_the_leading_number() {
        let parsed = parse_conversion_query("1,000 m in km").unwrap();
        assert_eq!(parsed.value, 1000.0);
    }

    #[test]
    fn not_a_conversion_query_returns_none() {
        assert_eq!(parse_conversion_query("(12*7)/4"), None);
        assert_eq!(parse_conversion_query("1+1"), None);
        assert_eq!(parse_conversion_query(""), None);
        assert_eq!(parse_conversion_query("hello world"), None);
    }

    #[test]
    fn a_leading_or_trailing_separator_is_not_a_query() {
        assert_eq!(parse_conversion_query("in km"), None);
        assert_eq!(parse_conversion_query("5 mi in"), None);
    }

    #[test]
    fn non_numeric_leading_token_is_not_a_query() {
        assert_eq!(parse_conversion_query("bananas mi in km"), None);
    }
}
