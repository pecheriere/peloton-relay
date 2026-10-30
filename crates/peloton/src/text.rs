//! Showing text that comes from outside.

/// `text`, which comes from Peloton or a relay, with every character that
/// must not reach a terminal or a page as is (control, format and line or
/// paragraph separator characters) replaced by U+FFFD, so it cannot drive or
/// reorder what is shown around it.
#[must_use]
pub fn shown(text: &str) -> String {
    text.chars()
        .map(|c| {
            if is_unsafe_to_show(c) {
                char::REPLACEMENT_CHARACTER
            } else {
                c
            }
        })
        .collect()
}

/// Whether `c` must not be shown as is: a control character
/// (`Cc`), a format character (`Cf`: bidi marks, embeddings, overrides and
/// isolates, zero-width and invisible characters, tags), or a line or
/// paragraph separator (`Zl`, `Zp`).
fn is_unsafe_to_show(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061c}'
                | '\u{06dd}'
                | '\u{070f}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08e2}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0001}'
                | '\u{e0020}'..='\u{e007f}'
        )
}

#[cfg(test)]
mod tests {
    use super::shown;

    #[test]
    fn shown_replaces_control_and_format_characters() {
        assert_eq!(shown("alice.rides"), "alice.rides");
        assert_eq!(
            shown("a\u{1b}b\u{202e}c\nd\u{2066}e\u{e0041}"),
            "a\u{fffd}b\u{fffd}c\u{fffd}d\u{fffd}e\u{fffd}"
        );
    }
}
