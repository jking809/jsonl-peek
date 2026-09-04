//! Single-pass health statistics over a JSONL stream, for the `stats` command.
//!
//! [`Stats::from_reader`] reads a [`BufRead`] one line at a time via
//! [`LineReader`] and, for every line, does exactly the work needed to answer
//! "is this dataset healthy": did it parse, what shape is the top level
//! value, which keys does it have and with what types, and - for any
//! [`FieldPath`] the caller asked about - how often is that field present and
//! what values does it take. Nothing here holds more than one line's worth of
//! JSON in memory at a time; running totals live in [`Histogram`] and a
//! handful of bounded tables.
//!
//! ```
//! use jsonl_peek::stats::{Stats, StatsOptions};
//!
//! let data = b"{\"role\":\"user\"}\n{\"role\":\"bot\"}\n{,}\n";
//! let stats = Stats::from_reader(&data[..], StatsOptions::default()).unwrap();
//! assert_eq!(stats.lines, 3);
//! assert_eq!(stats.valid, 2);
//! assert_eq!(stats.invalid(), 1);
//! assert_eq!(stats.issues[0].reason, "expected a quoted object key");
//! ```

use std::collections::HashMap;
use std::io::{self, BufRead};

use crate::hist::Histogram;
use crate::json::{self, Value};
use crate::lines::LineReader;
use crate::path::FieldPath;

/// Top level keys tracked before the table stops growing.
///
/// Datasets with a runaway number of distinct top level keys are usually
/// malformed (a schema-less blob per line, an accidental per-record UUID
/// key) rather than something worth cataloguing exhaustively, so the table
/// is bounded and [`Stats::keys_capped`] says when it stopped.
pub const MAX_KEYS: usize = 512;

/// Distinct values tracked per profiled field before its table stops
/// growing. See [`FieldStats::values_capped`].
pub const MAX_FIELD_VALUES: usize = 10_000;

/// Options controlling a [`Stats::from_reader`] pass.
#[derive(Debug, Clone)]
pub struct StatsOptions {
    /// Field paths to profile with a [`FieldStats`] entry each, in order.
    pub fields: Vec<FieldPath>,
    /// Broken lines kept in [`Stats::issues`] before the rest are only
    /// counted via [`Stats::issues_truncated`].
    pub max_errors: usize,
}

impl Default for StatsOptions {
    fn default() -> Self {
        StatsOptions {
            fields: Vec::new(),
            max_errors: 10,
        }
    }
}

/// A count of how many times each JSON type name has been seen.
#[derive(Debug, Clone, Default)]
pub struct TypeCounts {
    counts: Vec<(&'static str, u64)>,
}

impl TypeCounts {
    fn record(&mut self, type_name: &'static str) {
        match self.counts.iter_mut().find(|(t, _)| *t == type_name) {
            Some(entry) => entry.1 += 1,
            None => self.counts.push((type_name, 1)),
        }
    }

    /// The recorded types and their counts, in first-seen order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.counts.iter().copied()
    }

    /// The total number of values recorded, across every type.
    pub fn total(&self) -> u64 {
        self.counts.iter().map(|(_, c)| c).sum()
    }
}

/// One broken line: where it is and why it did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// 1-based line number.
    pub line: u64,
    /// 1-based byte column within the line.
    pub column: usize,
    /// A short, lowercase description of the problem.
    pub reason: String,
}

/// Occurrence and type counts for one top level object key.
#[derive(Debug, Clone)]
pub struct KeyStats {
    /// The key name.
    pub key: String,
    /// How many valid top-level objects contained this key.
    pub count: u64,
    /// The types of the values seen at this key.
    pub types: TypeCounts,
}

/// Presence, type and value-distribution stats for one profiled [`FieldPath`].
#[derive(Debug, Clone)]
pub struct FieldStats {
    /// The path this entry profiles.
    pub path: FieldPath,
    /// Records in which the path resolved to at least one value.
    pub present: u64,
    /// Total values resolved (more than `present` when the path fans out
    /// through a wildcard).
    pub values: u64,
    /// The types of every resolved value.
    pub types: TypeCounts,
    /// True once distinct value tracking hit [`MAX_FIELD_VALUES`] and
    /// further distinct values stopped being counted individually.
    pub values_capped: bool,
    value_counts: HashMap<String, u64>,
}

impl FieldStats {
    fn new(path: FieldPath) -> Self {
        FieldStats {
            path,
            present: 0,
            values: 0,
            types: TypeCounts::default(),
            values_capped: false,
            value_counts: HashMap::new(),
        }
    }

    fn record(&mut self, root: &Value) {
        let matches = self.path.resolve(root);
        if !matches.is_empty() {
            self.present += 1;
        }
        for value in matches {
            self.values += 1;
            self.types.record(value.type_name());
            let rendered = value.to_json();
            if let Some(count) = self.value_counts.get_mut(&rendered) {
                *count += 1;
            } else if self.value_counts.len() < MAX_FIELD_VALUES {
                self.value_counts.insert(rendered, 1);
            } else {
                self.values_capped = true;
            }
        }
    }

    /// How many distinct values have been tracked so far.
    pub fn distinct(&self) -> usize {
        self.value_counts.len()
    }

    /// The `n` most frequent values, as their compact JSON encoding, most
    /// frequent first. Ties break on the encoding itself, so the order is
    /// stable across runs.
    pub fn top(&self, n: usize) -> Vec<(String, u64)> {
        let mut items: Vec<(String, u64)> =
            self.value_counts.iter().map(|(v, c)| (v.clone(), *c)).collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        items.truncate(n);
        items
    }
}

/// The result of a [`Stats::from_reader`] pass over a JSONL stream.
#[derive(Debug, Clone)]
pub struct Stats {
    /// Total lines read, including blank and invalid ones.
    pub lines: u64,
    /// Lines that were empty or all whitespace.
    pub blank: u64,
    /// Lines that parsed as a complete JSON value.
    pub valid: u64,
    /// Total bytes read, including line endings.
    pub bytes: u64,
    /// The type of each valid line's top level value.
    pub top_level_types: TypeCounts,
    /// The byte length of every non-blank line.
    pub line_length: Histogram,
    /// True once the top level key table hit [`MAX_KEYS`] and further
    /// distinct keys stopped being tracked individually.
    pub keys_capped: bool,
    /// Broken lines, most recent first excluded - in the order encountered,
    /// up to `max_errors` from [`StatsOptions`].
    pub issues: Vec<Issue>,
    /// How many further broken lines were seen after `issues` filled up.
    pub issues_truncated: u64,
    /// Per-field results, in the same order as `StatsOptions::fields`.
    pub fields: Vec<FieldStats>,
    keys: HashMap<String, KeyStats>,
    max_errors: usize,
}

impl Stats {
    fn new(options: &StatsOptions) -> Self {
        Stats {
            lines: 0,
            blank: 0,
            valid: 0,
            bytes: 0,
            top_level_types: TypeCounts::default(),
            line_length: Histogram::new(),
            keys_capped: false,
            issues: Vec::new(),
            issues_truncated: 0,
            fields: options.fields.iter().cloned().map(FieldStats::new).collect(),
            keys: HashMap::new(),
            max_errors: options.max_errors,
        }
    }

    /// Runs a full pass over `reader`, splitting it into lines with
    /// [`LineReader`] and parsing each non-blank one.
    pub fn from_reader<R: BufRead>(reader: R, options: StatsOptions) -> io::Result<Stats> {
        let mut stats = Stats::new(&options);
        let mut lines = LineReader::new(reader);
        while let Some(line) = lines.read_line()? {
            stats.lines += 1;
            stats.bytes += line.raw_len as u64;
            if line.is_blank() {
                stats.blank += 1;
                continue;
            }
            stats.line_length.add(line.bytes.len() as u64);

            let text = match line.as_str() {
                Ok(text) => text,
                Err(err) => {
                    stats.record_issue(line.number, err.valid_up_to() + 1, "invalid UTF-8".to_string());
                    continue;
                }
            };
            match json::parse(text) {
                Ok(value) => {
                    stats.valid += 1;
                    stats.top_level_types.record(value.type_name());
                    if let Value::Object(members) = &value {
                        stats.record_keys(members);
                    }
                    for field in &mut stats.fields {
                        field.record(&value);
                    }
                }
                Err(err) => stats.record_issue(line.number, err.offset + 1, err.kind.to_string()),
            }
        }
        Ok(stats)
    }

    /// Lines that neither parsed nor were blank.
    pub fn invalid(&self) -> u64 {
        self.lines - self.blank - self.valid
    }

    /// Top level key statistics, most common key first. Ties break on the
    /// key name, so the order is stable across runs.
    pub fn keys(&self) -> Vec<&KeyStats> {
        let mut keys: Vec<&KeyStats> = self.keys.values().collect();
        keys.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        keys
    }

    fn record_issue(&mut self, line: u64, column: usize, reason: String) {
        if self.issues.len() < self.max_errors {
            self.issues.push(Issue { line, column, reason });
        } else {
            self.issues_truncated += 1;
        }
    }

    fn record_keys(&mut self, members: &[(String, Value)]) {
        for (key, value) in members {
            if let Some(existing) = self.keys.get_mut(key) {
                existing.count += 1;
                existing.types.record(value.type_name());
            } else if self.keys.len() < MAX_KEYS {
                let mut types = TypeCounts::default();
                types.record(value.type_name());
                self.keys.insert(
                    key.clone(),
                    KeyStats {
                        key: key.clone(),
                        count: 1,
                        types,
                    },
                );
            } else {
                self.keys_capped = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(data: &str, options: StatsOptions) -> Stats {
        Stats::from_reader(data.as_bytes(), options).unwrap()
    }

    #[test]
    fn counts_lines_blank_valid_and_invalid() {
        let stats = run("{\"a\":1}\n\nnot json\n{\"a\":2}\n", StatsOptions::default());
        assert_eq!(stats.lines, 4);
        assert_eq!(stats.blank, 1);
        assert_eq!(stats.valid, 2);
        assert_eq!(stats.invalid(), 1);
        assert_eq!(stats.bytes, "{\"a\":1}\n\nnot json\n{\"a\":2}\n".len() as u64);
    }

    #[test]
    fn records_parse_issues_with_line_and_column() {
        let stats = run("{\"a\":1}\n{\"a\":1,}\n", StatsOptions::default());
        assert_eq!(stats.issues.len(), 1);
        assert_eq!(stats.issues[0].line, 2);
        assert_eq!(stats.issues[0].column, 8);
        assert_eq!(stats.issues[0].reason, "expected a quoted object key");
    }

    #[test]
    fn records_invalid_utf8_as_an_issue() {
        let mut data = b"{\"a\":1}\n".to_vec();
        data.extend_from_slice(&[0xFF, 0xFE, b'\n']);
        let stats = Stats::from_reader(&data[..], StatsOptions::default()).unwrap();
        assert_eq!(stats.invalid(), 1);
        assert_eq!(stats.issues[0].line, 2);
        assert_eq!(stats.issues[0].reason, "invalid UTF-8");
    }

    #[test]
    fn truncates_issues_past_max_errors() {
        let data = "bad\n".repeat(5);
        let options = StatsOptions {
            max_errors: 2,
            ..StatsOptions::default()
        };
        let stats = run(&data, options);
        assert_eq!(stats.invalid(), 5);
        assert_eq!(stats.issues.len(), 2);
        assert_eq!(stats.issues_truncated, 3);
    }

    #[test]
    fn tracks_top_level_types() {
        let stats = run("1\n\"s\"\n[1]\n{}\n", StatsOptions::default());
        let types: HashMap<_, _> = stats.top_level_types.iter().collect();
        assert_eq!(types.get("int"), Some(&1));
        assert_eq!(types.get("string"), Some(&1));
        assert_eq!(types.get("array"), Some(&1));
        assert_eq!(types.get("object"), Some(&1));
    }

    #[test]
    fn tracks_key_presence_and_types() {
        let stats = run(
            "{\"id\":1,\"tags\":[1,2]}\n{\"id\":2}\n{\"id\":\"x\"}\n",
            StatsOptions::default(),
        );
        let keys = stats.keys();
        let id = keys.iter().find(|k| k.key == "id").unwrap();
        assert_eq!(id.count, 3);
        let id_types: HashMap<_, _> = id.types.iter().collect();
        assert_eq!(id_types.get("int"), Some(&2));
        assert_eq!(id_types.get("string"), Some(&1));

        let tags = keys.iter().find(|k| k.key == "tags").unwrap();
        assert_eq!(tags.count, 1);
        assert!(!stats.keys_capped);
    }

    #[test]
    fn caps_the_key_table_and_says_so() {
        let mut record = String::from("{");
        for i in 0..(MAX_KEYS + 10) {
            if i > 0 {
                record.push(',');
            }
            record.push_str(&format!("\"k{i}\":1"));
        }
        record.push_str("}\n");
        let stats = run(&record, StatsOptions::default());
        assert_eq!(stats.keys().len(), MAX_KEYS);
        assert!(stats.keys_capped);
    }

    #[test]
    fn profiles_a_field_path_present_and_missing() {
        let data = "{\"meta\":{\"source\":\"web\"}}\n{\"meta\":{\"source\":\"code\"}}\n{\"other\":1}\n";
        let options = StatsOptions {
            fields: vec![FieldPath::parse("meta.source").unwrap()],
            ..StatsOptions::default()
        };
        let stats = run(data, options);
        let field = &stats.fields[0];
        assert_eq!(field.present, 2);
        assert_eq!(field.values, 2);
        assert_eq!(field.distinct(), 2);
        let top = field.top(10);
        assert!(top.contains(&("\"web\"".to_string(), 1)));
        assert!(top.contains(&("\"code\"".to_string(), 1)));
    }

    #[test]
    fn profiles_a_wildcard_field_and_ranks_top_values() {
        let data = "{\"messages\":[{\"role\":\"user\"},{\"role\":\"assistant\"}]}\n\
                    {\"messages\":[{\"role\":\"user\"}]}\n";
        let options = StatsOptions {
            fields: vec![FieldPath::parse("messages[].role").unwrap()],
            ..StatsOptions::default()
        };
        let stats = run(data, options);
        let field = &stats.fields[0];
        assert_eq!(field.present, 2);
        assert_eq!(field.values, 3);
        assert_eq!(field.top(1), vec![("\"user\"".to_string(), 2)]);
    }

    #[test]
    fn caps_field_value_tracking_and_says_so() {
        let mut array = String::from("[");
        for i in 0..(MAX_FIELD_VALUES + 5) {
            if i > 0 {
                array.push(',');
            }
            array.push_str(&i.to_string());
        }
        array.push(']');
        let data = format!("{array}\n");
        let options = StatsOptions {
            fields: vec![FieldPath::parse("[]").unwrap()],
            ..StatsOptions::default()
        };
        let stats = run(&data, options);
        let field = &stats.fields[0];
        assert_eq!(field.values, (MAX_FIELD_VALUES + 5) as u64);
        assert_eq!(field.distinct(), MAX_FIELD_VALUES);
        assert!(field.values_capped);
    }

    #[test]
    fn line_length_histogram_matches_min_and_max() {
        let stats = run("{\"a\":1}\n{}\n", StatsOptions::default());
        assert_eq!(stats.line_length.min(), Some(2));
        assert_eq!(stats.line_length.max(), Some(7));
    }
}
