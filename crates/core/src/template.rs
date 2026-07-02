//! Placeholder handling. `{{fieldName}}` is the canonical syntax;
//! `##fieldName##` is accepted and normalized to it before rendering.

use regex::Regex;
use std::sync::LazyLock;

static HASH_PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"##([A-Za-z0-9_][A-Za-z0-9_ .\-]*?)##").unwrap());

static BRACE_PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([A-Za-z0-9_][A-Za-z0-9_ .\-]*?)\s*\}\}").unwrap());

/// Convert `##name##` placeholders to `{{name}}` so both syntaxes work.
pub fn normalize_placeholders(input: &str) -> String {
    HASH_PLACEHOLDER.replace_all(input, "{{$1}}").into_owned()
}

/// Extract unique placeholder names in order of first appearance.
/// Call on already-normalized text (subject + body).
pub fn extract_placeholders(input: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for capture in BRACE_PLACEHOLDER.captures_iter(input) {
        let name = capture[1].trim().to_string();
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hash_syntax() {
        assert_eq!(
            normalize_placeholders("Hi ##first_name##, ##product## is live"),
            "Hi {{first_name}}, {{product}} is live"
        );
    }

    #[test]
    fn leaves_brace_syntax_alone() {
        let text = "Hi {{first_name}}";
        assert_eq!(normalize_placeholders(text), text);
    }

    #[test]
    fn ignores_lone_hashes() {
        let text = "Price: #1 item ## nothing";
        assert_eq!(normalize_placeholders(text), text);
    }

    #[test]
    fn extracts_unique_in_order() {
        let text = "{{b}} {{ a }} {{b}} {{c}}";
        assert_eq!(extract_placeholders(text), vec!["b", "a", "c"]);
    }

    #[test]
    fn mixed_syntax_end_to_end() {
        let normalized = normalize_placeholders("##name## and {{city}}");
        assert_eq!(extract_placeholders(&normalized), vec!["name", "city"]);
    }
}
