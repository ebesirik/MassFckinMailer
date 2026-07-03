//! Field mapping (template placeholder ↔ file column) and per-row validation.

use crate::import::{RecipientTable, is_email};
use std::collections::{BTreeMap, HashSet};

/// Normalize a name for fuzzy matching: keep alphanumerics only, lowercased, so
/// `"First Name" == "first_name" == "firstName" == "first-name"`.
pub fn normalize_field(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Auto-map template fields to file columns by normalized-name equality.
/// Returns `field -> column` for confident matches only; unmatched fields are
/// omitted so the UI can prompt the user to map them manually.
pub fn auto_map(fields: &[String], columns: &[String]) -> BTreeMap<String, String> {
    let normalized_cols: Vec<(String, &String)> =
        columns.iter().map(|c| (normalize_field(c), c)).collect();

    let mut mapping = BTreeMap::new();
    for field in fields {
        let nf = normalize_field(field);
        if nf.is_empty() {
            continue;
        }
        if let Some((_, col)) = normalized_cols.iter().find(|(nc, _)| *nc == nf) {
            mapping.insert(field.clone(), (*col).clone());
        }
    }
    mapping
}

/// Build a `template field -> value` context for one row using the mapping.
/// Fields whose mapped column is missing from the table are skipped.
pub fn build_context(
    table: &RecipientTable,
    row: &[String],
    mapping: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    mapping
        .iter()
        .filter_map(|(field, column)| {
            let idx = table.column_index(column)?;
            let value = row.get(idx)?;
            Some((field.clone(), value.clone()))
        })
        .collect()
}

/// Outcome of validating a single recipient row. Each row falls into exactly
/// one bucket (checked in this order: email, duplicate, missing fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowStatus {
    Ok,
    /// Email cell is empty or not a valid address.
    InvalidEmail,
    /// A valid email that already appeared in an earlier row.
    Duplicate,
    /// Mapped template fields whose value in this row is empty.
    MissingFields(Vec<String>),
}

impl RowStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, RowStatus::Ok)
    }

    /// Short label for the preview table's status column.
    pub fn label(&self) -> &'static str {
        match self {
            RowStatus::Ok => "OK",
            RowStatus::InvalidEmail => "Bad email",
            RowStatus::Duplicate => "Duplicate",
            RowStatus::MissingFields(_) => "Missing data",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// One status per data row, aligned to `table.rows`.
    pub statuses: Vec<RowStatus>,
    pub valid: usize,
    pub invalid_email: usize,
    pub duplicates: usize,
    pub missing_fields: usize,
}

impl ValidationReport {
    /// Rows that would actually be sent when duplicates are kept.
    pub fn sendable(&self) -> usize {
        self.valid
    }

    /// Rows sendable when duplicates are treated as valid (de-dupe off).
    pub fn sendable_with_duplicates(&self) -> usize {
        self.valid + self.duplicates
    }
}

/// Validate every row. `email_col` is the column holding the address;
/// `required` lists the mapped template fields (name + column index) that must
/// be non-empty. Unmapped template fields are a configuration issue surfaced
/// separately by the caller, not a per-row error.
pub fn validate(
    table: &RecipientTable,
    email_col: usize,
    required: &[(String, usize)],
) -> ValidationReport {
    let mut report = ValidationReport {
        statuses: Vec::with_capacity(table.rows.len()),
        ..Default::default()
    };
    let mut seen: HashSet<String> = HashSet::new();

    for row in &table.rows {
        let email = row.get(email_col).map(|s| s.trim()).unwrap_or("");

        let status = if email.is_empty() || !is_email(email) {
            report.invalid_email += 1;
            RowStatus::InvalidEmail
        } else if !seen.insert(email.to_lowercase()) {
            report.duplicates += 1;
            RowStatus::Duplicate
        } else {
            let missing: Vec<String> = required
                .iter()
                .filter(|(_, col)| row.get(*col).map(|s| s.trim().is_empty()).unwrap_or(true))
                .map(|(name, _)| name.clone())
                .collect();
            if missing.is_empty() {
                report.valid += 1;
                RowStatus::Ok
            } else {
                report.missing_fields += 1;
                RowStatus::MissingFields(missing)
            }
        };
        report.statuses.push(status);
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(headers: &[&str], rows: &[&[&str]]) -> RecipientTable {
        RecipientTable {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|s| s.to_string()).collect())
                .collect(),
        }
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_field("First Name"), "firstname");
        assert_eq!(normalize_field("first_name"), "firstname");
        assert_eq!(normalize_field("first-name"), "firstname");
        assert_eq!(normalize_field("firstName"), "firstname");
        assert_eq!(normalize_field("  E-mail  "), "email");
    }

    #[test]
    fn auto_maps_by_normalized_name() {
        let fields = vec!["first_name".to_string(), "product".to_string()];
        let columns = vec![
            "First Name".to_string(),
            "E-mail".to_string(),
            "Product".to_string(),
        ];
        let mapping = auto_map(&fields, &columns);
        assert_eq!(mapping.get("first_name"), Some(&"First Name".to_string()));
        assert_eq!(mapping.get("product"), Some(&"Product".to_string()));
        assert_eq!(mapping.len(), 2);
    }

    #[test]
    fn auto_map_omits_unmatched() {
        let fields = vec!["nickname".to_string()];
        let columns = vec!["First Name".to_string()];
        assert!(auto_map(&fields, &columns).is_empty());
    }

    #[test]
    fn validates_all_buckets() {
        // columns: email(0), name(1)
        let t = table(
            &["email", "name"],
            &[
                &["a@x.com", "Ada"],  // Ok
                &["nope", "Bob"],     // InvalidEmail
                &["a@x.com", "Ada2"], // Duplicate (case-insensitive of row 0)
                &["c@z.com", ""],     // MissingFields([name])
                &["", "Dan"],         // InvalidEmail (empty)
            ],
        );
        let required = vec![("name".to_string(), 1usize)];
        let report = validate(&t, 0, &required);

        assert_eq!(report.statuses[0], RowStatus::Ok);
        assert_eq!(report.statuses[1], RowStatus::InvalidEmail);
        assert_eq!(report.statuses[2], RowStatus::Duplicate);
        assert_eq!(
            report.statuses[3],
            RowStatus::MissingFields(vec!["name".to_string()])
        );
        assert_eq!(report.statuses[4], RowStatus::InvalidEmail);

        assert_eq!(report.valid, 1);
        assert_eq!(report.invalid_email, 2);
        assert_eq!(report.duplicates, 1);
        assert_eq!(report.missing_fields, 1);
        assert_eq!(
            report.valid + report.invalid_email + report.duplicates + report.missing_fields,
            t.row_count()
        );
        assert_eq!(report.sendable(), 1);
        assert_eq!(report.sendable_with_duplicates(), 2);
    }

    #[test]
    fn builds_context_from_mapping() {
        let t = table(&["Email", "First Name"], &[&["a@x.com", "Ada"]]);
        let mapping: BTreeMap<String, String> =
            [("first_name".to_string(), "First Name".to_string())]
                .into_iter()
                .collect();
        let context = build_context(&t, &t.rows[0], &mapping);
        assert_eq!(context.get("first_name"), Some(&"Ada".to_string()));
        assert_eq!(context.len(), 1);
    }

    #[test]
    fn no_required_fields_passes() {
        let t = table(&["email"], &[&["a@x.com"], &["b@y.com"]]);
        let report = validate(&t, 0, &[]);
        assert_eq!(report.valid, 2);
    }

    /// Loading a project with an empty saved mapping should still resolve
    /// placeholders whose names match columns: the app merges `auto_map` under
    /// the saved mapping, so the preview fills in. (Mirrors `on_parsed`.)
    #[test]
    fn empty_saved_mapping_is_backfilled_by_auto_map() {
        let fields = vec!["name".to_string(), "email".to_string()];
        let t = table(
            &["email", "Name"],
            &[&["emre@besirik.com", "Emre At Besirik"]],
        );
        let saved: BTreeMap<String, String> = BTreeMap::new();

        // What `on_parsed` now does for a loaded project: auto-map, saved wins.
        let mut merged = auto_map(&fields, &t.headers);
        merged.extend(saved);

        let context = build_context(&t, &t.rows[0], &merged);
        assert_eq!(context.get("name"), Some(&"Emre At Besirik".to_string()));
        assert_eq!(context.get("email"), Some(&"emre@besirik.com".to_string()));
    }
}
