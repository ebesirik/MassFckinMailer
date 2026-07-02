//! Recipient list import: CSV/XLSX parsing into a rectangular table plus
//! email-column detection over sample rows. Pure and sync — no tokio; callers
//! run this on gpui's background executor so the UI never blocks.

use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

pub static EMAIL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^@\s]+@[^@\s]+\.[^@\s]{2,}$").unwrap());

pub fn is_email(value: &str) -> bool {
    EMAIL_RE.is_match(value.trim())
}

/// Minimum fraction of non-empty sample values that must look like emails
/// for a column to qualify.
const MIN_MATCH_RATIO: f64 = 0.8;

/// Detect which column holds email addresses by scanning sample data rows
/// (headers excluded). Returns the column index with the highest match ratio
/// that reaches [`MIN_MATCH_RATIO`]. The user can always override in the UI.
pub fn detect_email_column(sample_rows: &[Vec<String>]) -> Option<usize> {
    let columns = sample_rows.iter().map(|row| row.len()).max()?;
    let mut best: Option<(usize, f64)> = None;

    for col in 0..columns {
        let mut non_empty = 0usize;
        let mut matches = 0usize;
        for row in sample_rows {
            let Some(value) = row.get(col) else { continue };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            non_empty += 1;
            if is_email(value) {
                matches += 1;
            }
        }
        if non_empty == 0 {
            continue;
        }
        let ratio = matches as f64 / non_empty as f64;
        if ratio >= MIN_MATCH_RATIO && best.is_none_or(|(_, r)| ratio > r) {
            best = Some((col, ratio));
        }
    }

    best.map(|(col, _)| col)
}

/// Convenience: detect the email column over the first [`SAMPLE_ROWS`] rows.
pub fn detect_email_column_in(table: &RecipientTable) -> Option<usize> {
    detect_email_column(table.sample(SAMPLE_ROWS))
}

/// How many data rows to scan when detecting the email column.
pub const SAMPLE_ROWS: usize = 20;

/// A recipient list parsed into a rectangular table: `headers` names each
/// column and every row is padded/truncated to `headers.len()` cells.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecipientTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl RecipientTable {
    pub fn column_count(&self) -> usize {
        self.headers.len()
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Index of the column with this exact header name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.headers.iter().position(|h| h == name)
    }

    /// The first `n` data rows (for detection/preview), fewer if the table is smaller.
    pub fn sample(&self, n: usize) -> &[Vec<String>] {
        &self.rows[..self.rows.len().min(n)]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Csv,
    Excel,
}

/// Classify a file by extension. Returns `None` for unsupported types.
pub fn source_kind(path: &Path) -> Option<SourceKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "csv" | "tsv" | "txt" => Some(SourceKind::Csv),
        "xlsx" | "xls" | "xlsm" | "xlsb" | "ods" => Some(SourceKind::Excel),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("failed to read file: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV parse error: {0}")]
    Csv(#[from] csv::Error),
    #[error("spreadsheet error: {0}")]
    Spreadsheet(String),
    #[error("the spreadsheet has no sheets")]
    NoSheets,
    #[error("the file has no header row")]
    Empty,
    #[error("unsupported file type: .{0}")]
    UnsupportedType(String),
}

/// Delimiters CSV sniffing will consider, in preference order on ties.
const CSV_DELIMITERS: [u8; 3] = [b',', b';', b'\t'];

/// Guess the delimiter from the first non-blank line: whichever candidate
/// appears most often wins, defaulting to comma when none are present.
pub fn sniff_delimiter(sample: &str) -> u8 {
    let line = sample.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let count = |d: u8| line.matches(d as char).count();
    CSV_DELIMITERS
        .into_iter()
        .filter(|&d| count(d) > 0)
        .max_by_key(|&d| count(d))
        .unwrap_or(b',')
}

/// Parse CSV bytes, sniffing the delimiter from the header line.
pub fn parse_csv_bytes(bytes: &[u8]) -> Result<RecipientTable, ImportError> {
    let head = &bytes[..bytes.len().min(8192)];
    let delimiter = sniff_delimiter(&String::from_utf8_lossy(head));
    parse_csv_with(bytes, delimiter)
}

/// Parse CSV bytes with an explicit delimiter. First record is the header row;
/// ragged rows are padded/truncated to header width and blank rows dropped.
pub fn parse_csv_with(bytes: &[u8], delimiter: u8) -> Result<RecipientTable, ImportError> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);

    let mut records = reader.records();
    let headers: Vec<String> = match records.next() {
        Some(record) => record?.iter().map(|s| s.trim().to_string()).collect(),
        None => return Err(ImportError::Empty),
    };
    let width = headers.len();

    let mut rows = Vec::new();
    for record in records {
        let mut row: Vec<String> = record?.iter().map(|s| s.to_string()).collect();
        set_width(&mut row, width);
        if row.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        rows.push(row);
    }
    Ok(RecipientTable { headers, rows })
}

/// Sheet names in a spreadsheet, for the sheet picker.
pub fn excel_sheet_names(path: &Path) -> Result<Vec<String>, ImportError> {
    use calamine::Reader as _;
    let workbook =
        calamine::open_workbook_auto(path).map_err(|e| ImportError::Spreadsheet(e.to_string()))?;
    Ok(workbook.sheet_names())
}

/// Parse one sheet (the first if `sheet` is `None`). First row = headers.
pub fn parse_excel(path: &Path, sheet: Option<&str>) -> Result<RecipientTable, ImportError> {
    use calamine::Reader as _;
    let mut workbook =
        calamine::open_workbook_auto(path).map_err(|e| ImportError::Spreadsheet(e.to_string()))?;

    let name = match sheet {
        Some(s) => s.to_string(),
        None => workbook
            .sheet_names()
            .first()
            .cloned()
            .ok_or(ImportError::NoSheets)?,
    };
    let range = workbook
        .worksheet_range(&name)
        .map_err(|e| ImportError::Spreadsheet(e.to_string()))?;

    let mut iter = range.rows();
    let headers: Vec<String> = match iter.next() {
        Some(row) => row
            .iter()
            .map(|c| c.to_string().trim().to_string())
            .collect(),
        None => return Err(ImportError::Empty),
    };
    let width = headers.len();

    let mut rows = Vec::new();
    for row in iter {
        let mut r: Vec<String> = row.iter().map(|c| c.to_string()).collect();
        set_width(&mut r, width);
        if r.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        rows.push(r);
    }
    Ok(RecipientTable { headers, rows })
}

/// Parse any supported file. For Excel, `sheet` selects the worksheet.
pub fn parse_file(path: &Path, sheet: Option<&str>) -> Result<RecipientTable, ImportError> {
    match source_kind(path) {
        Some(SourceKind::Csv) => parse_csv_bytes(&std::fs::read(path)?),
        Some(SourceKind::Excel) => parse_excel(path, sheet),
        None => Err(ImportError::UnsupportedType(
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string(),
        )),
    }
}

fn set_width(row: &mut Vec<String>, width: usize) {
    if row.len() < width {
        row.resize(width, String::new());
    } else {
        row.truncate(width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(data: &[&[&str]]) -> Vec<Vec<String>> {
        data.iter()
            .map(|row| row.iter().map(|value| value.to_string()).collect())
            .collect()
    }

    #[test]
    fn detects_email_column() {
        let sample = rows(&[
            &["Alice", "alice@example.com", "Berlin"],
            &["Bob", "bob@test.org", "Oslo"],
            &["Carol", "carol@mail.co", "Rome"],
        ]);
        assert_eq!(detect_email_column(&sample), Some(1));
    }

    #[test]
    fn tolerates_some_bad_values() {
        let sample = rows(&[
            &["a@x.com"],
            &["b@y.org"],
            &["c@z.net"],
            &["d@w.io"],
            &["not-an-email"],
        ]);
        assert_eq!(detect_email_column(&sample), Some(0));
    }

    #[test]
    fn rejects_below_threshold() {
        let sample = rows(&[&["a@x.com"], &["nope"], &["also nope"]]);
        assert_eq!(detect_email_column(&sample), None);
    }

    #[test]
    fn skips_empty_values() {
        let sample = rows(&[&["", "a@x.com"], &["", "b@y.org"], &["", ""]]);
        assert_eq!(detect_email_column(&sample), Some(1));
    }

    #[test]
    fn email_regex_basics() {
        assert!(is_email("user@example.com"));
        assert!(is_email("  padded@example.com  "));
        assert!(!is_email("user@nodot"));
        assert!(!is_email("two words@example.com"));
        assert!(!is_email("@example.com"));
    }

    #[test]
    fn parses_comma_csv() {
        let csv = "First Name,E-mail,City\nAlice,alice@example.com,Berlin\nBob,bob@test.org,Oslo\n";
        let table = parse_csv_bytes(csv.as_bytes()).unwrap();
        assert_eq!(table.headers, vec!["First Name", "E-mail", "City"]);
        assert_eq!(table.row_count(), 2);
        assert_eq!(table.rows[0], vec!["Alice", "alice@example.com", "Berlin"]);
        assert_eq!(table.column_index("E-mail"), Some(1));
        assert_eq!(detect_email_column_in(&table), Some(1));
    }

    #[test]
    fn sniffs_semicolon_and_tab() {
        assert_eq!(sniff_delimiter("a;b;c\n1;2;3"), b';');
        assert_eq!(sniff_delimiter("a\tb\tc"), b'\t');
        assert_eq!(sniff_delimiter("a,b,c"), b',');
        assert_eq!(sniff_delimiter("single"), b',');
    }

    #[test]
    fn parses_semicolon_csv() {
        let csv = "name;email\nAda;ada@x.com\n";
        let table = parse_csv_bytes(csv.as_bytes()).unwrap();
        assert_eq!(table.headers, vec!["name", "email"]);
        assert_eq!(table.rows[0], vec!["Ada", "ada@x.com"]);
    }

    #[test]
    fn pads_and_drops_blank_rows() {
        // Second data row is short; a fully blank line is skipped.
        let csv = "a,b,c\n1,2,3\n4,5\n\n7,8,9\n";
        let table = parse_csv_bytes(csv.as_bytes()).unwrap();
        assert_eq!(table.row_count(), 3);
        assert_eq!(table.rows[1], vec!["4", "5", ""]); // padded to width 3
    }

    #[test]
    fn empty_input_is_error() {
        assert!(matches!(parse_csv_bytes(b""), Err(ImportError::Empty)));
    }

    #[test]
    fn source_kind_by_extension() {
        assert_eq!(source_kind(Path::new("a.csv")), Some(SourceKind::Csv));
        assert_eq!(source_kind(Path::new("a.XLSX")), Some(SourceKind::Excel));
        assert_eq!(source_kind(Path::new("a.pdf")), None);
        assert_eq!(source_kind(Path::new("noext")), None);
    }
}
