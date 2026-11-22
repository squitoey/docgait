//! Search over names and paths (the names index level). The query language is simple by default:
//! words must all appear (in the file name or its folder path), with optional filters.
//!
//! | Input | Meaning |
//! | --- | --- |
//! | `lease renewal` | all words, anywhere in name or path |
//! | `"lease renewal"` | exact phrase |
//! | `-draft`, `-"old copy"` | exclude |
//! | `type:pdf` / `kind:image` | extension / kind |
//! | `in:Taxes` | under a folder whose path contains the text |
//! | `after:2024-01 before:2024-06` | modified date (year, month or day) |
//! | `created-after:… created-before:…` | created date |
//! | `size:>10mb`, `size:<1kb` | size |
//!
//! Content search, `tag:`, `has:` and `person:` arrive with later milestones; until then they
//! are reported back as unsupported rather than silently ignored.

use anyhow::Result;
use rusqlite::types::Value;
use serde::Serialize;

use crate::library::{row_to_file, FileEntry};
use crate::Library;

/// Trigram search needs at least this many characters per term; shorter ones use LIKE.
const MIN_TRIGRAM: usize = 3;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Query {
    pub terms: Vec<String>,
    pub excluded: Vec<String>,
    pub kinds: Vec<String>,
    pub exts: Vec<String>,
    pub in_folders: Vec<String>,
    pub tags: Vec<String>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub created_after: Option<i64>,
    pub created_before: Option<i64>,
    pub size_min: Option<i64>,
    pub size_max: Option<i64>,
    /// Filters this build cannot apply yet, or could not understand.
    pub unsupported: Vec<String>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.kinds.is_empty()
            && self.exts.is_empty()
            && self.in_folders.is_empty()
            && self.tags.is_empty()
            && self.modified_after.is_none()
            && self.modified_before.is_none()
            && self.created_after.is_none()
            && self.created_before.is_none()
            && self.size_min.is_none()
            && self.size_max.is_none()
    }
}

const KINDS: &[&str] = &[
    "document", "image", "audio", "video", "ebook", "archive", "code", "package", "other",
];

pub fn parse(input: &str) -> Query {
    let mut q = Query::default();
    for tok in tokenize(input) {
        let (negated, body) = match tok.text.strip_prefix('-') {
            Some(rest) if !tok.quoted_whole && !rest.is_empty() => (true, rest.to_string()),
            _ => (tok.negated_quote, tok.text.clone()),
        };
        // key:value filters (never for a quoted phrase).
        if !tok.quoted_whole {
            if let Some((key, value)) = body.split_once(':') {
                let key = key.to_ascii_lowercase();
                let value = value.trim_matches('"').to_string();
                if apply_filter(&mut q, &key, &value) {
                    continue;
                }
                if is_known_later(&key) {
                    q.unsupported.push(format!("{key}: (not available yet)"));
                    continue;
                }
            }
        }
        let term = body.trim().to_string();
        if term.is_empty() {
            continue;
        }
        if negated {
            q.excluded.push(term);
        } else {
            q.terms.push(term);
        }
    }
    q
}

fn is_known_later(key: &str) -> bool {
    matches!(key, "has" | "person")
}

fn apply_filter(q: &mut Query, key: &str, value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    match key {
        "type" | "ext" => {
            if KINDS.contains(&v.as_str()) {
                q.kinds.push(v);
            } else {
                q.exts.push(v.trim_start_matches('.').to_string());
            }
        }
        "kind" => q.kinds.push(v),
        "in" => q.in_folders.push(value.to_string()),
        "tag" => q.tags.push(value.to_string()),
        "after" | "modified-after" => match parse_date(&v) {
            Some((start, _)) => q.modified_after = Some(start),
            None => q.unsupported.push(format!(
                "{key}:{value} (dates look like 2024, 2024-06 or 2024-06-30)"
            )),
        },
        "before" | "modified-before" => match parse_date(&v) {
            Some((start, _)) => q.modified_before = Some(start),
            None => q.unsupported.push(format!(
                "{key}:{value} (dates look like 2024, 2024-06 or 2024-06-30)"
            )),
        },
        "created-after" => match parse_date(&v) {
            Some((start, _)) => q.created_after = Some(start),
            None => q.unsupported.push(format!(
                "{key}:{value} (dates look like 2024, 2024-06 or 2024-06-30)"
            )),
        },
        "created-before" => match parse_date(&v) {
            Some((start, _)) => q.created_before = Some(start),
            None => q.unsupported.push(format!(
                "{key}:{value} (dates look like 2024, 2024-06 or 2024-06-30)"
            )),
        },
        "size" => {
            if !parse_size_filter(q, &v) {
                q.unsupported
                    .push(format!("size:{value} (try size:>10mb or size:<1kb)"));
            }
        }
        _ => return false,
    }
    true
}

struct Token {
    text: String,
    /// The whole token was a quoted phrase.
    quoted_whole: bool,
    /// `-"phrase"`.
    negated_quote: bool,
}

/// Splits on whitespace, keeping "quoted phrases" (also after `-` or `key:`) together.
fn tokenize(input: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut text = String::new();
        let mut quoted_whole = false;
        let mut negated_quote = false;
        if c == '"' || (c == '-' && input_after_is_quote(&chars)) {
            if c == '-' {
                chars.next();
                negated_quote = true;
            }
            chars.next(); // opening quote
            for ch in chars.by_ref() {
                if ch == '"' {
                    break;
                }
                text.push(ch);
            }
            quoted_whole = true;
        } else {
            let mut in_quote = false;
            while let Some(&ch) = chars.peek() {
                if ch == '"' {
                    in_quote = !in_quote;
                    text.push(ch);
                    chars.next();
                    continue;
                }
                if ch.is_whitespace() && !in_quote {
                    break;
                }
                text.push(ch);
                chars.next();
            }
        }
        out.push(Token {
            text,
            quoted_whole,
            negated_quote,
        });
    }
    out
}

fn input_after_is_quote(chars: &std::iter::Peekable<std::str::Chars>) -> bool {
    let mut it = chars.clone();
    it.next();
    it.next() == Some('"')
}

/// `2024`, `2024-06` or `2024-06-30` → (start, end) in ns since the epoch, UTC.
fn parse_date(s: &str) -> Option<(i64, i64)> {
    let parts: Vec<&str> = s.split('-').collect();
    let y: i64 = parts.first()?.parse().ok()?;
    if !(1900..=9999).contains(&y) {
        return None;
    }
    let (m, d, unit) = match parts.len() {
        1 => (1, 1, 'y'),
        2 => (parts[1].parse().ok()?, 1, 'm'),
        3 => (parts[1].parse().ok()?, parts[2].parse().ok()?, 'd'),
        _ => return None,
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let start = days_from_civil(y, m, d) * 86_400;
    let end = match unit {
        'y' => days_from_civil(y + 1, 1, 1) * 86_400,
        'm' if m == 12 => days_from_civil(y + 1, 1, 1) * 86_400,
        'm' => days_from_civil(y, m + 1, 1) * 86_400,
        _ => start + 86_400,
    };
    Some((start * 1_000_000_000, end * 1_000_000_000))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_size_filter(q: &mut Query, s: &str) -> bool {
    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = s.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = s.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = s.strip_prefix('<') {
        ("<", r)
    } else {
        (">=", s)
    };
    let Some(bytes) = parse_bytes(rest) else {
        return false;
    };
    match op {
        ">" => q.size_min = Some(bytes + 1),
        ">=" => q.size_min = Some(bytes),
        "<" => q.size_max = Some(bytes - 1),
        _ => q.size_max = Some(bytes),
    }
    true
}

fn parse_bytes(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024f64.powi(4),
        _ => return None,
    };
    Some((n * mult) as i64)
}

#[derive(Serialize, Clone, Debug)]
pub struct SearchHit {
    #[serde(flatten)]
    pub file: FileEntry,
    pub source_id: i64,
    /// Folder path relative to the source root ('/' separated).
    pub folder_path: String,
    pub source_root: String,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    /// More matches exist beyond `limit`.
    pub more: bool,
    /// Filters that were not applied (not available yet, or not understood).
    pub unsupported: Vec<String>,
}

/// A trigram MATCH expression requiring every term (each as a phrase).
fn match_expr(terms: &[&String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn like_pattern(t: &str) -> String {
    let escaped = t
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

impl Library {
    pub fn search(&self, input: &str, limit: usize) -> Result<SearchResults> {
        let q = parse(input);
        let mut out = SearchResults {
            unsupported: q.unsupported.clone(),
            ..Default::default()
        };
        if q.is_empty() {
            return Ok(out);
        }
        let limit = limit.clamp(1, 5000);
        let (long, short): (Vec<&String>, Vec<&String>) = q
            .terms
            .iter()
            .partition(|t| t.chars().count() >= MIN_TRIGRAM);
        let (long_ex, short_ex): (Vec<&String>, Vec<&String>) = q
            .excluded
            .iter()
            .partition(|t| t.chars().count() >= MIN_TRIGRAM);

        let mut sql = String::from(
            "SELECT f.id, f.folder_id, f.name, f.ext, f.kind, f.size, f.mtime_ns, f.state,
                    f.source_id, d.rel_path, s.root
               FROM file f
               JOIN folder d ON d.id = f.folder_id
               JOIN source s ON s.id = f.source_id",
        );
        let mut args: Vec<Value> = Vec::new();
        let ranked = !long.is_empty();
        if ranked {
            // Name hits rank above path-only hits.
            sql.push_str(
                " JOIN (SELECT rowid AS id, bm25(file_fts, 10.0, 1.0) AS rank
                         FROM file_fts WHERE file_fts MATCH ?) m ON m.id = f.id",
            );
            args.push(Value::Text(match_expr(&long)));
        }
        sql.push_str(" WHERE f.state = 'present'");
        for t in &short {
            sql.push_str(" AND (f.name LIKE ? ESCAPE '\\' OR d.rel_path LIKE ? ESCAPE '\\')");
            let p = like_pattern(t);
            args.push(Value::Text(p.clone()));
            args.push(Value::Text(p));
        }
        if !long_ex.is_empty() {
            sql.push_str(" AND f.id NOT IN (SELECT rowid FROM file_fts WHERE file_fts MATCH ?)");
            let any: Vec<String> = long_ex
                .iter()
                .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
                .collect();
            args.push(Value::Text(any.join(" OR ")));
        }
        for t in &short_ex {
            sql.push_str(" AND NOT (f.name LIKE ? ESCAPE '\\' OR d.rel_path LIKE ? ESCAPE '\\')");
            let p = like_pattern(t);
            args.push(Value::Text(p.clone()));
            args.push(Value::Text(p));
        }
        if !q.kinds.is_empty() {
            sql.push_str(&format!(" AND f.kind IN ({})", placeholders(q.kinds.len())));
            args.extend(q.kinds.iter().map(|k| Value::Text(k.clone())));
        }
        if !q.exts.is_empty() {
            sql.push_str(&format!(" AND f.ext IN ({})", placeholders(q.exts.len())));
            args.extend(q.exts.iter().map(|e| Value::Text(e.clone())));
        }
        for tag in &q.tags {
            sql.push_str(
                " AND f.id IN (SELECT ft.file_id FROM file_tag ft JOIN tag t ON t.id = ft.tag_id
                               WHERE t.name = ? COLLATE NOCASE)",
            );
            args.push(Value::Text(tag.clone()));
        }
        for folder in &q.in_folders {
            sql.push_str(" AND (d.rel_path LIKE ? ESCAPE '\\' OR s.root LIKE ? ESCAPE '\\')");
            let p = like_pattern(folder);
            args.push(Value::Text(p.clone()));
            args.push(Value::Text(p));
        }
        let mut bound = |cond: &str, v: Option<i64>| {
            if let Some(v) = v {
                sql.push_str(cond);
                args.push(Value::Integer(v));
            }
        };
        bound(" AND f.mtime_ns >= ?", q.modified_after);
        bound(" AND f.mtime_ns < ?", q.modified_before);
        bound(" AND f.created_ns >= ?", q.created_after);
        bound(" AND f.created_ns < ?", q.created_before);
        bound(" AND f.size >= ?", q.size_min);
        bound(" AND f.size <= ?", q.size_max);
        sql.push_str(if ranked {
            " ORDER BY m.rank, f.mtime_ns DESC LIMIT ?"
        } else {
            " ORDER BY f.mtime_ns DESC LIMIT ?"
        });
        args.push(Value::Integer(limit as i64 + 1));

        let conn = self.reader()?;
        let mut stmt = conn.prepare(&sql)?;
        let mut hits = stmt
            .query_map(rusqlite::params_from_iter(args), |r| {
                Ok(SearchHit {
                    file: row_to_file(r)?,
                    source_id: r.get(8)?,
                    folder_path: r.get(9)?,
                    source_root: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.more = hits.len() > limit;
        hits.truncate(limit);
        out.hits = hits;
        Ok(out)
    }
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_terms_phrases_and_exclusions() {
        let q = parse(r#"lease "renewal notice" -draft -"old copy""#);
        assert_eq!(q.terms, vec!["lease", "renewal notice"]);
        assert_eq!(q.excluded, vec!["draft", "old copy"]);
    }

    #[test]
    fn parses_filters() {
        let q = parse("w2 type:pdf kind:image in:Taxes after:2024-01 before:2024 size:>10mb");
        assert_eq!(q.terms, vec!["w2"]);
        assert_eq!(q.exts, vec!["pdf"]);
        assert_eq!(q.kinds, vec!["image"]);
        assert_eq!(q.in_folders, vec!["Taxes"]);
        assert_eq!(
            q.modified_after,
            Some(days_from_civil(2024, 1, 1) * 86_400 * 1_000_000_000)
        );
        assert_eq!(
            q.modified_before,
            Some(days_from_civil(2024, 1, 1) * 86_400 * 1_000_000_000)
        );
        assert_eq!(q.size_min, Some(10 * 1024 * 1024 + 1));
        assert!(q.unsupported.is_empty());
    }

    #[test]
    fn type_accepts_kind_names_and_quoted_folders() {
        let q = parse(r#"type:image in:"Tax Returns""#);
        assert_eq!(q.kinds, vec!["image"]);
        assert_eq!(q.in_folders, vec!["Tax Returns"]);
    }

    #[test]
    fn later_filters_are_reported() {
        let q = parse("person:jane has:ocr after:yesterday");
        assert_eq!(q.unsupported.len(), 3);
        assert!(q.is_empty());
        assert_eq!(parse("tag:receipts").tags, vec!["receipts"]);
    }

    #[test]
    fn colons_in_plain_words_are_kept() {
        let q = parse("12:30");
        assert_eq!(q.terms, vec!["12:30"]);
    }

    #[test]
    fn dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert!(parse_date("2024-13").is_none());
        let (s, e) = parse_date("2024-02").unwrap();
        assert_eq!((e - s) / 86_400 / 1_000_000_000, 29);
    }
}
