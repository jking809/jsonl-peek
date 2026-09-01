//! Field path parsing and resolution, for `--field` and future schema use.
//!
//! A path is a dot-separated chain of object member names, each optionally
//! followed by one or more `[...]` array accessors: an index (`[0]`, `[-1]`),
//! or a wildcard (`[]`) that fans out to every element. A path may also start
//! with a bare accessor (`[0].id`) for records that are themselves arrays.
//!
//! ```
//! use jsonl_peek::json::parse;
//! use jsonl_peek::path::FieldPath;
//!
//! let path = FieldPath::parse("messages[].role").unwrap();
//! let record = parse(r#"{"messages":[{"role":"user"},{"role":"assistant"}]}"#).unwrap();
//! let roles: Vec<_> = path.resolve(&record).iter().filter_map(|v| v.as_str()).collect();
//! assert_eq!(roles, ["user", "assistant"]);
//! ```

use crate::json::Value;
use std::fmt;

/// One step in a [`FieldPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// An object member name.
    Key(String),
    /// A single array element. Negative indices count from the end, so `-1`
    /// is the last element.
    Index(i64),
    /// Every element of an array.
    Wildcard,
}

/// A parsed field path, as written on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    raw: String,
    segments: Vec<Segment>,
}

impl FieldPath {
    /// Parses a path such as `meta.source` or `messages[].role`.
    pub fn parse(input: &str) -> Result<FieldPath, PathError> {
        let bytes = input.as_bytes();
        let mut i = 0;
        let mut segments = Vec::new();
        let mut first = true;

        loop {
            let seg_start = i;
            let name_start = i;
            while i < bytes.len() && !matches!(bytes[i], b'.' | b'[' | b']') {
                i += 1;
            }
            let name = &input[name_start..i];
            if name.is_empty() {
                if !first {
                    return Err(PathError {
                        position: seg_start,
                        message: "empty path segment".to_string(),
                    });
                }
            } else {
                segments.push(Segment::Key(name.to_string()));
            }

            let mut had_bracket = false;
            while i < bytes.len() && bytes[i] == b'[' {
                had_bracket = true;
                i += 1;
                let idx_start = i;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(PathError {
                        position: idx_start - 1,
                        message: "unterminated '['".to_string(),
                    });
                }
                let idx_str = &input[idx_start..i];
                i += 1; // consume ']'
                if idx_str.is_empty() {
                    segments.push(Segment::Wildcard);
                } else {
                    match idx_str.parse::<i64>() {
                        Ok(n) => segments.push(Segment::Index(n)),
                        Err(_) => {
                            return Err(PathError {
                                position: idx_start,
                                message: format!("invalid array index '{idx_str}'"),
                            })
                        }
                    }
                }
            }

            if name.is_empty() && !had_bracket {
                return Err(PathError {
                    position: seg_start,
                    message: "empty field path".to_string(),
                });
            }

            first = false;
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                if i == bytes.len() {
                    return Err(PathError {
                        position: i,
                        message: "trailing '.'".to_string(),
                    });
                }
                continue;
            }
            break;
        }

        if i != bytes.len() {
            let bad = input[i..].chars().next().unwrap();
            return Err(PathError {
                position: i,
                message: format!("unexpected '{bad}'"),
            });
        }

        Ok(FieldPath {
            raw: input.to_string(),
            segments,
        })
    }

    /// The path's segments, in order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Every value the path reaches inside `root`.
    ///
    /// A wildcard or an index into a value that is not an array simply
    /// contributes nothing, rather than being an error: most records in a
    /// dataset will not match every path, and that absence is exactly what
    /// `present in N of M records` is meant to report.
    pub fn resolve<'v>(&self, root: &'v Value) -> Vec<&'v Value> {
        let mut current: Vec<&Value> = vec![root];
        for segment in &self.segments {
            let mut next = Vec::new();
            for value in current {
                match segment {
                    Segment::Key(key) => {
                        if let Some(found) = value.get(key) {
                            next.push(found);
                        }
                    }
                    Segment::Index(index) => {
                        if let Some(array) = value.as_array() {
                            if let Some(i) = normalize_index(*index, array.len()) {
                                next.push(&array[i]);
                            }
                        }
                    }
                    Segment::Wildcard => {
                        if let Some(array) = value.as_array() {
                            next.extend(array.iter());
                        }
                    }
                }
            }
            current = next;
        }
        current
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Turns a possibly-negative array index into a slice offset, or `None` if it
/// is out of range.
fn normalize_index(index: i64, len: usize) -> Option<usize> {
    if index >= 0 {
        let i = index as usize;
        (i < len).then_some(i)
    } else {
        let resolved = len as i64 + index;
        (resolved >= 0).then_some(resolved as usize)
    }
}

/// A malformed field path, together with the byte offset that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError {
    /// Zero based byte offset into the parsed path.
    pub position: usize,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at position {}: {}", self.position, self.message)
    }
}

impl std::error::Error for PathError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse as parse_json;

    #[test]
    fn parses_a_bare_key() {
        let path = FieldPath::parse("role").unwrap();
        assert_eq!(path.segments(), [Segment::Key("role".to_string())]);
    }

    #[test]
    fn parses_a_dotted_path() {
        let path = FieldPath::parse("meta.source").unwrap();
        assert_eq!(
            path.segments(),
            [
                Segment::Key("meta".to_string()),
                Segment::Key("source".to_string()),
            ]
        );
    }

    #[test]
    fn parses_an_index() {
        let path = FieldPath::parse("messages[0].content").unwrap();
        assert_eq!(
            path.segments(),
            [
                Segment::Key("messages".to_string()),
                Segment::Index(0),
                Segment::Key("content".to_string()),
            ]
        );
    }

    #[test]
    fn parses_a_negative_index() {
        let path = FieldPath::parse("messages[-1].content").unwrap();
        assert_eq!(
            path.segments(),
            [
                Segment::Key("messages".to_string()),
                Segment::Index(-1),
                Segment::Key("content".to_string()),
            ]
        );
    }

    #[test]
    fn parses_a_wildcard() {
        let path = FieldPath::parse("messages[].role").unwrap();
        assert_eq!(
            path.segments(),
            [
                Segment::Key("messages".to_string()),
                Segment::Wildcard,
                Segment::Key("role".to_string()),
            ]
        );
    }

    #[test]
    fn parses_a_leading_index() {
        let path = FieldPath::parse("[0].id").unwrap();
        assert_eq!(
            path.segments(),
            [Segment::Index(0), Segment::Key("id".to_string())]
        );
    }

    #[test]
    fn display_returns_the_original_text() {
        assert_eq!(FieldPath::parse("messages[].role").unwrap().to_string(), "messages[].role");
    }

    #[test]
    fn rejects_empty_path() {
        assert_eq!(FieldPath::parse("").unwrap_err().message, "empty field path");
    }

    #[test]
    fn rejects_trailing_dot() {
        let e = FieldPath::parse("meta.").unwrap_err();
        assert_eq!(e.message, "trailing '.'");
        assert_eq!(e.position, 5);
    }

    #[test]
    fn rejects_double_dot() {
        let e = FieldPath::parse("a..b").unwrap_err();
        assert_eq!(e.message, "empty path segment");
        assert_eq!(e.position, 2);
    }

    #[test]
    fn rejects_unterminated_bracket() {
        let e = FieldPath::parse("messages[0").unwrap_err();
        assert_eq!(e.message, "unterminated '['");
    }

    #[test]
    fn rejects_non_numeric_index() {
        let e = FieldPath::parse("messages[x]").unwrap_err();
        assert_eq!(e.message, "invalid array index 'x'");
    }

    #[test]
    fn rejects_stray_close_bracket() {
        let e = FieldPath::parse("a]").unwrap_err();
        assert_eq!(e.message, "unexpected ']'");
        assert_eq!(e.position, 1);
    }

    #[test]
    fn resolves_a_simple_key() {
        let record = parse_json(r#"{"role":"user"}"#).unwrap();
        let path = FieldPath::parse("role").unwrap();
        assert_eq!(path.resolve(&record), [&Value::Str("user".to_string())]);
    }

    #[test]
    fn resolves_through_nested_objects() {
        let record = parse_json(r#"{"meta":{"source":"web"}}"#).unwrap();
        let path = FieldPath::parse("meta.source").unwrap();
        assert_eq!(path.resolve(&record), [&Value::Str("web".to_string())]);
    }

    #[test]
    fn resolves_a_wildcard_to_every_element() {
        let record = parse_json(r#"{"messages":[{"role":"user"},{"role":"assistant"}]}"#).unwrap();
        let path = FieldPath::parse("messages[].role").unwrap();
        let values: Vec<&str> = path.resolve(&record).iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(values, ["user", "assistant"]);
    }

    #[test]
    fn resolves_negative_index_from_the_end() {
        let record = parse_json(r#"{"messages":[{"role":"user"},{"role":"assistant"}]}"#).unwrap();
        let path = FieldPath::parse("messages[-1].role").unwrap();
        assert_eq!(path.resolve(&record), [&Value::Str("assistant".to_string())]);
    }

    #[test]
    fn out_of_range_index_resolves_to_nothing() {
        let record = parse_json(r#"{"messages":[1,2]}"#).unwrap();
        assert!(FieldPath::parse("messages[5]").unwrap().resolve(&record).is_empty());
        assert!(FieldPath::parse("messages[-5]").unwrap().resolve(&record).is_empty());
    }

    #[test]
    fn missing_key_resolves_to_nothing() {
        let record = parse_json(r#"{"a":1}"#).unwrap();
        assert!(FieldPath::parse("b").unwrap().resolve(&record).is_empty());
    }

    #[test]
    fn index_into_non_array_resolves_to_nothing() {
        let record = parse_json(r#"{"a":1}"#).unwrap();
        assert!(FieldPath::parse("a[0]").unwrap().resolve(&record).is_empty());
    }

    #[test]
    fn resolves_a_leading_index() {
        let record = parse_json(r#"[{"id":1},{"id":2}]"#).unwrap();
        let path = FieldPath::parse("[1].id").unwrap();
        assert_eq!(path.resolve(&record), [&Value::Int(2)]);
    }
}
