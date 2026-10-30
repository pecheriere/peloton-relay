//! Host names as the relay and the login helper accept them.

/// Whether `name` is a DNS name of letters, digits and hyphens: dot-separated
/// labels of 1 to 63 lowercase ASCII letters, digits and hyphens, none
/// starting or ending with a hyphen. An internationalised name passes only in
/// its `xn--` form, as `url` writes it.
#[must_use]
pub fn is_dns_name(name: &str) -> bool {
    name.split('.').all(|label| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

#[cfg(test)]
mod tests {
    use super::is_dns_name;

    #[test]
    fn dns_names_are_ldh_labels() {
        for good in [
            "relay",
            "relay.example.net",
            "a-1.b2.c",
            "xn--rly-0ma.example",
        ] {
            assert!(is_dns_name(good), "{good}");
        }
        let long = "a".repeat(64);
        for bad in [
            "",
            ".",
            "relay.",
            "relay..net",
            "-relay.net",
            "relay-.net",
            "Relay.net",
            "a$(id).net",
            "a&b.net",
            "a_b.net",
            long.as_str(),
        ] {
            assert!(!is_dns_name(bad), "{bad:?}");
        }
    }
}
