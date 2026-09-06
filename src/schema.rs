//! Single-pass schema discovery over a JSONL stream, for the `schema` command.
//!
//! Where [`crate::stats::FieldStats`] profiles paths the caller already knows
//! to ask about, [`Schema`] finds them: it walks every record's JSON tree up
//! to a bounded depth and, for each distinct path it encounters, tracks how
//! many records contain it and what type of value shows up there. A path
//! that resolves to two types across the file (`int:1990 string:10`) is
//! exactly the kind of thing that breaks a loader partway through a run.
//!
//! Array elements share one path (`messages[]`, not `messages[0]`,
//! `messages[1]`, ...), so a field common to every message counts once per
//! record for `present` and once per element for `occurrences` - the same
//! present/occurrences split [`crate::stats::FieldStats`] uses for a
//! wildcard path.
//!
//! ```
//! use jsonl_peek::schema::{Schema, SchemaOptions};
//!
//! let data = b"{\"role\":\"user\",\"meta\":{\"n\":1}}\n{\"role\":\"bot\"}\n{,}\n";
//! let schema = Schema::from_reader(&data[..], SchemaOptions::default()).unwrap();
//! assert_eq!(schema.records, 2);
//! assert_eq!(schema.unparseable, 1);
//! let meta_n = schema.paths().into_iter().find(|p| p.path == "meta.n").unwrap();
//! assert_eq!(meta_n.present, 1);
//! ```

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead};

use crate::json::{self, Value};
use crate::lines::LineReader;
use crate::stats::TypeCounts;

/// Distinct paths tracked before the table stops growing.
///
/// A per-record UUID key or similarly unbounded key space would otherwise
/// grow this table without limit; past this point further paths are only
/// reflected in [`Schema::paths_capped`].
pub const MAX_PATHS: usize = 2000;

/// Options controlling a [`Schema::from_reader`] pass.
#[derive(Debug, Clone)]
pub struct SchemaOptions {
    /// How many path segments to descend. A key or an array's `[]` accessor
    /// each count as one segment, so `messages[].role` is depth 3.
    pub depth: usize,
    /// Paths present in fewer than this fraction of records are left out of
    /// [`Schema::paths`]. `0.0` keeps everything.
    pub min_rate: f64,
}

impl Default for SchemaOptions {
    fn default() -> Self {
        SchemaOptions {
            depth: 3,
            min_rate: 0.0,
        }
    }
}

/// Presence and type counts for one path discovered in the data.
#[derive(Debug, Clone)]
pub struct PathStats {
    /// The path, e.g. `meta.source` or `messages[].role`.
    pub path: String,
    /// Records in which this path resolved to at least one value.
    pub present: u64,
    /// Total values seen at this path (more than `present` when it runs
    /// through an array).
    pub occurrences: u64,
    /// The types of every value seen at this path.
    pub types: TypeCounts,
}

/// The result of a [`Schema::from_reader`] pass over a JSONL stream.
#[derive(Debug, Clone)]
pub struct Schema {
    /// Records that parsed as a complete JSON value.
    pub records: u64,
    /// Lines that were neither blank nor parsed.
    pub unparseable: u64,
    /// True once the path table hit [`MAX_PATHS`] and further distinct paths
    /// stopped being tracked individually.
    pub paths_capped: bool,
    paths: HashMap<String, PathStats>,
    depth: usize,
    min_rate: f64,
}

impl Schema {
    fn new(options: &SchemaOptions) -> Self {
        Schema {
            records: 0,
            unparseable: 0,
            paths_capped: false,
            paths: HashMap::new(),
            depth: options.depth,
            min_rate: options.min_rate,
        }
    }

    /// Runs a full pass over `reader`, splitting it into lines with
    /// [`LineReader`] and walking each parsed record's tree.
    pub fn from_reader<R: BufRead>(reader: R, options: SchemaOptions) -> io::Result<Schema> {
        let mut schema = Schema::new(&options);
        let mut lines = LineReader::new(reader);
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(line) = lines.read_line()? {
            if line.is_blank() {
                continue;
            }
            let text = match line.as_str() {
                Ok(text) => text,
                Err(_) => {
                    schema.unparseable += 1;
                    continue;
                }
            };
            match json::parse(text) {
                Ok(value) => {
                    schema.records += 1;
                    seen.clear();
                    schema.walk(&value, "", 0, &mut seen);
                }
                Err(_) => schema.unparseable += 1,
            }
        }
        Ok(schema)
    }

    fn walk(&mut self, value: &Value, path: &str, depth: usize, seen: &mut HashSet<String>) {
        match value {
            Value::Object(members) => {
                for (key, child) in members {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    self.visit(child, child_path, depth + 1, seen);
                }
            }
            Value::Array(items) => {
                let child_path = format!("{path}[]");
                for item in items {
                    self.visit(item, child_path.clone(), depth + 1, seen);
                }
            }
            _ => {}
        }
    }

    fn visit(&mut self, value: &Value, child_path: String, child_depth: usize, seen: &mut HashSet<String>) {
        if child_depth > self.depth {
            return;
        }
        let first_in_record = seen.insert(child_path.clone());
        self.record(&child_path, value, first_in_record);
        if child_depth < self.depth {
            self.walk(value, &child_path, child_depth, seen);
        }
    }

    fn record(&mut self, path: &str, value: &Value, first_in_record: bool) {
        if let Some(existing) = self.paths.get_mut(path) {
            existing.occurrences += 1;
            existing.types.record(value.type_name());
            if first_in_record {
                existing.present += 1;
            }
        } else if self.paths.len() < MAX_PATHS {
            let mut types = TypeCounts::default();
            types.record(value.type_name());
            self.paths.insert(
                path.to_string(),
                PathStats {
                    path: path.to_string(),
                    present: u64::from(first_in_record),
                    occurrences: 1,
                    types,
                },
            );
        } else {
            self.paths_capped = true;
        }
    }

    /// How often `stats` was present in a record, as a fraction in `[0, 1]`.
    /// `0.0` if no records have been read.
    pub fn rate(&self, stats: &PathStats) -> f64 {
        if self.records == 0 {
            0.0
        } else {
            stats.present as f64 / self.records as f64
        }
    }

    /// Discovered paths meeting `min_rate`, ordered so that a path always
    /// follows its parent (plain lexicographic order, since `.` sorts before
    /// `[`).
    pub fn paths(&self) -> Vec<&PathStats> {
        let mut paths: Vec<&PathStats> = self
            .paths
            .values()
            .filter(|p| self.rate(p) >= self.min_rate)
            .collect();
        paths.sort_by(|a, b| a.path.cmp(&b.path));
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(data: &str, options: SchemaOptions) -> Schema {
        Schema::from_reader(data.as_bytes(), options).unwrap()
    }

    fn find<'a>(schema: &'a Schema, path: &str) -> &'a PathStats {
        schema
            .paths()
            .into_iter()
            .find(|p| p.path == path)
            .unwrap_or_else(|| panic!("no path {path}"))
    }

    #[test]
    fn discovers_flat_keys() {
        let schema = run("{\"id\":1,\"role\":\"user\"}\n", SchemaOptions::default());
        assert_eq!(schema.records, 1);
        let id = find(&schema, "id");
        assert_eq!(id.present, 1);
        assert_eq!(id.occurrences, 1);
        assert_eq!(id.types.iter().collect::<Vec<_>>(), [("int", 1)]);
    }

    #[test]
    fn descends_into_nested_objects() {
        let schema = run("{\"meta\":{\"source\":\"web\"}}\n", SchemaOptions::default());
        let meta = find(&schema, "meta");
        assert_eq!(meta.types.iter().collect::<Vec<_>>(), [("object", 1)]);
        let source = find(&schema, "meta.source");
        assert_eq!(source.present, 1);
        assert_eq!(source.types.iter().collect::<Vec<_>>(), [("string", 1)]);
    }

    #[test]
    fn arrays_share_one_path_and_count_every_element() {
        let data = "{\"messages\":[{\"role\":\"user\"},{\"role\":\"bot\"}]}\n{\"messages\":[{\"role\":\"user\"}]}\n";
        let schema = run(data, SchemaOptions::default());
        let elements = find(&schema, "messages[]");
        assert_eq!(elements.present, 2);
        assert_eq!(elements.occurrences, 3);
        let role = find(&schema, "messages[].role");
        assert_eq!(role.present, 2);
        assert_eq!(role.occurrences, 3);
    }

    #[test]
    fn stops_descending_past_the_depth_limit() {
        let data = "{\"a\":{\"b\":{\"c\":1}}}\n";
        let schema = run(data, SchemaOptions { depth: 2, min_rate: 0.0 });
        assert!(schema.paths().iter().any(|p| p.path == "a.b"));
        assert!(!schema.paths().iter().any(|p| p.path == "a.b.c"));
    }

    #[test]
    fn zero_depth_records_nothing() {
        let schema = run("{\"a\":1}\n", SchemaOptions { depth: 0, min_rate: 0.0 });
        assert!(schema.paths().is_empty());
    }

    #[test]
    fn min_rate_hides_sparse_paths() {
        let data = "{\"a\":1,\"tags\":1}\n{\"a\":2}\n{\"a\":3}\n{\"a\":4}\n";
        let schema = run(
            data,
            SchemaOptions {
                depth: 3,
                min_rate: 0.5,
            },
        );
        assert!(schema.paths().iter().any(|p| p.path == "a"));
        assert!(!schema.paths().iter().any(|p| p.path == "tags"));
    }

    #[test]
    fn counts_unparseable_lines_and_skips_blanks() {
        let schema = run("{\"a\":1}\n\n{,}\nnot json\n", SchemaOptions::default());
        assert_eq!(schema.records, 1);
        assert_eq!(schema.unparseable, 2);
    }

    #[test]
    fn caps_the_path_table_and_says_so() {
        let mut record = String::from("{");
        for i in 0..(MAX_PATHS + 10) {
            if i > 0 {
                record.push(',');
            }
            record.push_str(&format!("\"k{i}\":1"));
        }
        record.push_str("}\n");
        let schema = run(&record, SchemaOptions::default());
        assert_eq!(schema.paths().len(), MAX_PATHS);
        assert!(schema.paths_capped);
    }

    #[test]
    fn paths_are_sorted_parent_before_child() {
        let data = "{\"messages\":[{\"role\":\"user\"}],\"id\":1}\n";
        let schema = run(data, SchemaOptions::default());
        let names: Vec<&str> = schema.paths().iter().map(|p| p.path.as_str()).collect();
        assert_eq!(names, ["id", "messages", "messages[]", "messages[].role"]);
    }
}
