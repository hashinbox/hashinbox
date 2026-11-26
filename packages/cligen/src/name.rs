//! Case conversion, `heck`'s, never starting with a digit. Each language
//! escapes its own reserved words.

use heck::{ToLowerCamelCase, ToPascalCase, ToSnakeCase};

/// `read_wallet` → `ReadWallet`.
#[must_use]
pub fn pascal(text: &str) -> String {
    start(text.to_pascal_case(), "V")
}

/// `read_wallet` → `readWallet`.
#[must_use]
pub fn camel(text: &str) -> String {
    start(text.to_lower_camel_case(), "v")
}

/// `ExternalId` → `external_id`.
#[must_use]
pub fn snake(text: &str) -> String {
    start(text.to_snake_case(), "v")
}

/// `name`, prefixed when empty or starting with a digit.
fn start(name: String, prefix: &str) -> String {
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("{prefix}{name}")
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases() {
        assert_eq!(pascal("read_wallet"), "ReadWallet");
        assert_eq!(pascal("1h"), "V1h");
        assert_eq!(camel("read_wallet"), "readWallet");
        assert_eq!(snake("ExternalId"), "external_id");
    }
}
