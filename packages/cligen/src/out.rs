//! Text helpers every language writes with: a list fitted to a width,
//! quoting, identifiers.

/// How a formatter lays out a line: its width and one level of indentation.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    /// The line width.
    pub width: usize,
    /// One level of indentation.
    pub unit: &'static str,
}

/// `head`, `items` between the brackets, then `tail`: on one line when it
/// fits from `column`, else one item per line, a level deeper, each with a
/// trailing comma (how prettier and rustfmt break a list). `spaced` pads
/// the brackets on one line: `{ a, b }`.
#[must_use]
pub fn fit(
    style: Style,
    column: usize,
    head: &str,
    (open, close): (&str, &str),
    items: &[String],
    tail: &str,
    spaced: bool,
) -> String {
    let pad = if spaced && !items.is_empty() { " " } else { "" };
    let flat = format!("{head}{open}{pad}{}{pad}{close}{tail}", items.join(", "));
    if items.is_empty() || column + flat.trim_end_matches('\n').chars().count() <= style.width {
        return flat;
    }
    let items: String = items
        .iter()
        .map(|item| format!("{}{item},\n", style.unit))
        .collect();
    format!("{head}{open}\n{items}{close}{tail}")
}

/// `text` as a double-quoted literal, escaped as JSON escapes it.
#[must_use]
pub fn quote(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

/// Whether `text` is an ASCII identifier.
#[must_use]
pub fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_or_breaks() {
        let style = Style {
            width: 12,
            unit: "  ",
        };
        let items = ["a".to_owned(), "b".to_owned()];
        assert_eq!(
            fit(style, 0, "f", ("(", ")"), &items, ";", false),
            "f(a, b);"
        );
        assert_eq!(
            fit(style, 6, "f", ("(", ")"), &items, ";", false),
            "f(\n  a,\n  b,\n);"
        );
    }
}
