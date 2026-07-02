//! Placeholder handling and rendering. `{{fieldName}}` is the canonical syntax;
//! `##fieldName##` is accepted and normalized to it before rendering. Rendering
//! uses `minijinja` in a restricted, strict environment: an undefined variable
//! is an error surfaced before sending, never a silent blank.

use minijinja::{Environment, UndefinedBehavior};
use regex::Regex;
use std::collections::BTreeMap;
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

/// Turn a column header into a placeholder identifier safe for `{{ }}`
/// (minijinja variable names can't contain spaces). `"First Name" -> "first_name"`,
/// `"E-mail" -> "e_mail"`. Used when inserting placeholder chips.
pub fn to_placeholder_ident(header: &str) -> String {
    let mut out = String::with_capacity(header.len());
    let mut prev_underscore = false;
    for ch in header.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "field".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// One or more placeholders have no value in the context. Holds the joined
    /// list of missing field names.
    #[error("missing value for: {0}")]
    Missing(String),
    #[error("template syntax error: {0}")]
    Syntax(String),
    #[error("render error: {0}")]
    Other(String),
}

/// Render a template with `{{field}}`/`##field##` placeholders against a
/// `field -> value` context. Any placeholder without a value is reported up
/// front (with its name) rather than rendered blank.
pub fn render(
    template_src: &str,
    context: &BTreeMap<String, String>,
) -> Result<String, RenderError> {
    let normalized = normalize_placeholders(template_src);

    // Precise, friendly missing-field detection for our simple placeholders.
    let missing: Vec<String> = extract_placeholders(&normalized)
        .into_iter()
        .filter(|name| !context.contains_key(name))
        .collect();
    if !missing.is_empty() {
        return Err(RenderError::Missing(missing.join(", ")));
    }

    // Fresh environment with no loaders (no file/include access) and strict
    // undefined handling as a backstop for anything the precheck didn't catch.
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    let template = env
        .template_from_str(&normalized)
        .map_err(|e| RenderError::Syntax(e.to_string()))?;
    template
        .render(context)
        .map_err(|e| RenderError::Other(e.to_string()))
}

static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
static LINE_BREAK: LazyLock<Regex> = LazyLock::new(|| {
    // Block-level closings / breaks become newlines in the plain-text version.
    Regex::new(r"(?i)<br\s*/?>|</(p|div|h[1-6]|li|tr)>").unwrap()
});
static MANY_NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

/// Best-effort HTML → plain text for the auto-generated text alternative.
/// Not a full HTML parser: turns block breaks into newlines, strips remaining
/// tags, and decodes the common entities.
pub fn html_to_text(html: &str) -> String {
    let with_breaks = LINE_BREAK.replace_all(html, "\n");
    let stripped = HTML_TAG.replace_all(&with_breaks, "");
    let decoded = stripped
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    // Trim trailing spaces per line, collapse runs of blank lines.
    let trimmed: String = decoded
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    MANY_NEWLINES
        .replace_all(&trimmed, "\n\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn renders_with_context() {
        let out = render(
            "Hi {{first_name}}, {{product}} is live!",
            &ctx(&[("first_name", "Ada"), ("product", "Widget")]),
        )
        .unwrap();
        assert_eq!(out, "Hi Ada, Widget is live!");
    }

    #[test]
    fn renders_hash_syntax() {
        let out = render("Hi ##name##", &ctx(&[("name", "Bob")])).unwrap();
        assert_eq!(out, "Hi Bob");
    }

    #[test]
    fn missing_field_errors_with_name() {
        let err = render("Hi {{first_name}} {{city}}", &ctx(&[("first_name", "Ada")])).unwrap_err();
        match err {
            RenderError::Missing(names) => assert_eq!(names, "city"),
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn placeholder_idents() {
        assert_eq!(to_placeholder_ident("First Name"), "first_name");
        assert_eq!(to_placeholder_ident("E-mail"), "e_mail");
        assert_eq!(to_placeholder_ident("  Product  "), "product");
        assert_eq!(to_placeholder_ident("!!!"), "field");
    }

    #[test]
    fn html_to_text_basics() {
        let html = "<p>Hi <b>Ada</b>,</p><p>Welcome &amp; enjoy</p><br>Bye";
        let text = html_to_text(html);
        assert_eq!(text, "Hi Ada,\nWelcome & enjoy\n\nBye");
    }

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
