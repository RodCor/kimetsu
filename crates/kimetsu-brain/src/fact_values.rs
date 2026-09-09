//! Exact equivalence keys for bounded numeric configuration values.

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn decimal(number: &str) -> Option<(u128, u128)> {
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (number.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let denominator = 10_u128.checked_pow(fraction.len().try_into().ok()?)?;
    let numerator = whole
        .parse::<u128>()
        .ok()?
        .checked_mul(denominator)?
        .checked_add(if fraction.is_empty() {
            0
        } else {
            fraction.parse().ok()?
        })?;
    let divisor = gcd(numerator, denominator);
    Some((numerator / divisor, denominator / divisor))
}

fn scaled(number: &str, top: u128, bottom: u128) -> Option<(u128, u128)> {
    let (mut numerator, mut denominator) = decimal(number)?;
    // Cross-cancel before multiplication, including zero, to avoid needless overflow.
    let cancel_top = gcd(top, denominator);
    denominator /= cancel_top;
    let cancel_bottom = gcd(numerator, bottom);
    numerator /= cancel_bottom;
    Some((
        numerator.checked_mul(top / cancel_top)?,
        denominator.checked_mul(bottom / cancel_bottom)?,
    ))
}

fn numeric_key(attribute: &str, value: &str) -> Option<String> {
    if value.len() > 128 {
        return None;
    }
    let input = value.trim();
    if matches!(attribute, "port" | "retries" | "replicas") {
        if input.is_empty() || !input.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let number = input.parse::<u128>().ok()?;
        if attribute == "port" && number > u16::MAX.into() {
            return None;
        }
        return Some(format!("integer:{number}"));
    }
    if !matches!(attribute, "timeout" | "retention" | "memory_limit") {
        return None;
    }
    let split = input.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let number = &input[..split];
    let unit = input[split..].trim().to_ascii_lowercase();
    let (dimension, top, bottom) = if attribute == "memory_limit" {
        (
            "bytes",
            match unit.as_str() {
                "bytes" => 1,
                "kb" => 1_000,
                "mb" => 1_000_000,
                "gb" => 1_000_000_000,
                "kib" => 1_024,
                "mib" => 1_048_576,
                "gib" => 1_073_741_824,
                _ => return None,
            },
            1,
        )
    } else {
        let (top, bottom) = match unit.as_str() {
            "ms" | "millisecond" | "milliseconds" | "milisegundo" | "milisegundos" => (1, 1_000),
            "s" | "second" | "seconds" | "segundo" | "segundos" => (1, 1),
            "minute" | "minutes" | "minuto" | "minutos" => (60, 1),
            "hour" | "hours" | "hora" | "horas" => (3_600, 1),
            "day" | "days" | "dia" | "dias" => (86_400, 1),
            "week" | "weeks" | "semana" | "semanas" => (604_800, 1),
            _ => return None,
        };
        ("seconds", top, bottom)
    };
    let (numerator, denominator) = scaled(number, top, bottom)?;
    Some(format!("{dimension}:{numerator}/{denominator}"))
}

/// Comparison-only key; never replace a displayed value with this key.
/// Numeric parsing is bounded to 128 bytes and checked u128 rational arithmetic.
/// Unsupported units (including calendar months/years), malformed numbers and
/// overflow retain an exact, case-sensitive key in a separate namespace.
pub fn equivalence_key(attribute: &str, value: &str) -> String {
    numeric_key(attribute, value).unwrap_or_else(|| format!("exact:{value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_durations_share_keys_without_rounding() {
        for (left, right) in [
            ("30 seconds", "30000 ms"),
            ("0.5s", "500ms"),
            ("1 minute", "60 seconds"),
            ("1.25 hours", "75 minutes"),
            ("21 days", "3 weeks"),
            ("0.0001ms", "0.0000001s"),
        ] {
            assert_eq!(
                equivalence_key("timeout", left),
                equivalence_key("timeout", right),
                "{left} / {right}"
            );
        }
        assert_ne!(
            equivalence_key("timeout", "0.0000001s"),
            equivalence_key("timeout", "0.0000002s")
        );
        assert_eq!(
            equivalence_key("retention", "12 horas"),
            equivalence_key("retention", "720 minutos")
        );
    }

    #[test]
    fn binary_and_decimal_memory_units_are_distinct() {
        assert_eq!(
            equivalence_key("memory_limit", "0.5 MiB"),
            equivalence_key("memory_limit", "524288 bytes")
        );
        assert_eq!(
            equivalence_key("memory_limit", "1 GB"),
            equivalence_key("memory_limit", "1000 MB")
        );
        assert_ne!(
            equivalence_key("memory_limit", "1 MiB"),
            equivalence_key("memory_limit", "1 MB")
        );
    }

    #[test]
    fn integer_attributes_normalize_only_valid_counts() {
        for attribute in ["port", "retries", "replicas"] {
            assert_eq!(
                equivalence_key(attribute, "00042"),
                equivalence_key(attribute, "42")
            );
            assert_ne!(
                equivalence_key(attribute, "4.2"),
                equivalence_key(attribute, "42")
            );
        }
    }

    #[test]
    fn unsupported_values_and_sensitive_strings_remain_exact() {
        for attribute in ["password", "encryption_key", "cache.key", "version"] {
            assert_ne!(
                equivalence_key(attribute, "AbC"),
                equivalence_key(attribute, "abc")
            );
            assert_ne!(
                equivalence_key(attribute, "01"),
                equivalence_key(attribute, "1")
            );
        }
        for (left, right) in [
            ("1 month", "30 days"),
            ("1 year", "365 days"),
            ("1..0s", "1s"),
            ("-1s", "1s"),
            ("1e3ms", "1s"),
            ("1s extra", "1s"),
            ("340282366920938463463374607431768211456s", "0s"),
        ] {
            assert_ne!(
                equivalence_key("timeout", left),
                equivalence_key("timeout", right)
            );
        }
    }
}
