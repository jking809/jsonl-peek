//! A reusable-buffer line splitter for JSONL input.
//!
//! `stats`, `schema`, `head` and `sample` all need the same thing: split a
//! `BufRead` into records without allocating per line, so that a 40 GB file
//! costs one read buffer instead of one `String` per record. [`LineReader`]
//! does that split and also deals with the three things that break a naive
//! `read_line` loop over real-world JSONL: a UTF-8 BOM on the first line,
//! CRLF endings, and a final record with no trailing newline at all.
//!
//! ```
//! use jsonl_peek::lines::LineReader;
//!
//! let mut reader = LineReader::new("{\"a\":1}\r\n{\"b\":2}".as_bytes());
//! let first = reader.read_line().unwrap().unwrap();
//! assert_eq!(first.bytes, b"{\"a\":1}");
//! let second = reader.read_line().unwrap().unwrap();
//! assert_eq!(second.bytes, b"{\"b\":2}");
//! assert!(reader.read_line().unwrap().is_none());
//! ```

use std::io::{self, BufRead};
use std::str::Utf8Error;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Splits a [`BufRead`] into lines, reusing one internal buffer.
///
/// Each call to [`read_line`](LineReader::read_line) overwrites the buffer
/// from the previous call, so a returned [`Line`] borrows the reader and
/// must be consumed (or copied) before the next call.
pub struct LineReader<R> {
    reader: R,
    buf: Vec<u8>,
    number: u64,
    bom_checked: bool,
}

impl<R: BufRead> LineReader<R> {
    /// Wraps a reader. Nothing is read until the first call to
    /// [`read_line`](LineReader::read_line).
    pub fn new(reader: R) -> Self {
        LineReader {
            reader,
            buf: Vec::new(),
            number: 0,
            bom_checked: false,
        }
    }

    /// Reads the next line, or `None` at end of input.
    ///
    /// The trailing `\n` (and a preceding `\r`, if present) is stripped. A
    /// UTF-8 BOM at the very start of the input is stripped from the first
    /// line only. A final line with no trailing newline is still returned.
    pub fn read_line(&mut self) -> io::Result<Option<Line<'_>>> {
        self.buf.clear();
        let raw_len = self.reader.read_until(b'\n', &mut self.buf)?;
        if raw_len == 0 {
            return Ok(None);
        }
        self.number += 1;

        let mut start = 0;
        if !self.bom_checked {
            self.bom_checked = true;
            if self.buf.starts_with(&BOM) {
                start = BOM.len();
            }
        }

        let mut end = self.buf.len();
        if end > 0 && self.buf[end - 1] == b'\n' {
            end -= 1;
            if end > start && self.buf[end - 1] == b'\r' {
                end -= 1;
            }
        }

        Ok(Some(Line {
            number: self.number,
            bytes: &self.buf[start..end],
            raw_len,
        }))
    }
}

/// One line of input, with its 1-based line number.
pub struct Line<'a> {
    /// 1-based line number, counting blank lines and unparseable lines.
    pub number: u64,
    /// The line's content, with any trailing newline (and BOM, on line 1)
    /// already stripped.
    pub bytes: &'a [u8],
    /// Bytes consumed from the underlying reader to produce this line,
    /// including the trailing newline. Summing this across every line
    /// yielded reconstructs the exact byte length of the input, which is
    /// what makes it possible to report a total for a pipe as well as a
    /// file.
    pub raw_len: usize,
}

impl<'a> Line<'a> {
    /// True if the line is empty or contains only ASCII whitespace.
    pub fn is_blank(&self) -> bool {
        self.bytes.iter().all(u8::is_ascii_whitespace)
    }

    /// Decodes the line as UTF-8.
    pub fn as_str(&self) -> Result<&'a str, Utf8Error> {
        std::str::from_utf8(self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &str) -> Vec<String> {
        let mut reader = LineReader::new(input.as_bytes());
        let mut out = Vec::new();
        while let Some(line) = reader.read_line().unwrap() {
            out.push(line.as_str().unwrap().to_string());
        }
        out
    }

    #[test]
    fn splits_on_newline() {
        assert_eq!(lines("a\nb\nc\n"), vec!["a", "b", "c"]);
    }

    #[test]
    fn keeps_final_line_without_trailing_newline() {
        assert_eq!(lines("a\nb"), vec!["a", "b"]);
    }

    #[test]
    fn strips_crlf() {
        assert_eq!(lines("a\r\nb\r\n"), vec!["a", "b"]);
    }

    #[test]
    fn strips_bom_from_first_line_only() {
        let input = format!("{}a\nb\n", std::str::from_utf8(&BOM).unwrap());
        assert_eq!(lines(&input), vec!["a", "b"]);
    }

    #[test]
    fn preserves_empty_lines_and_numbers_them() {
        let mut reader = LineReader::new("a\n\nb\n".as_bytes());
        let l1 = reader.read_line().unwrap().unwrap();
        assert_eq!((l1.number, l1.bytes), (1, b"a".as_slice()));
        let l2 = reader.read_line().unwrap().unwrap();
        assert_eq!((l2.number, l2.bytes), (2, b"".as_slice()));
        assert!(l2.is_blank());
        let l3 = reader.read_line().unwrap().unwrap();
        assert_eq!((l3.number, l3.bytes), (3, b"b".as_slice()));
        assert!(reader.read_line().unwrap().is_none());
    }

    #[test]
    fn whitespace_only_line_is_blank() {
        let mut reader = LineReader::new("  \t \n".as_bytes());
        let line = reader.read_line().unwrap().unwrap();
        assert!(line.is_blank());
    }

    #[test]
    fn raw_len_includes_the_newline() {
        let mut reader = LineReader::new("ab\nc".as_bytes());
        let l1 = reader.read_line().unwrap().unwrap();
        assert_eq!(l1.bytes.len(), 2);
        assert_eq!(l1.raw_len, 3);
        let l2 = reader.read_line().unwrap().unwrap();
        assert_eq!(l2.bytes.len(), 1);
        assert_eq!(l2.raw_len, 1);
    }

    #[test]
    fn reports_invalid_utf8() {
        let mut reader = LineReader::new(&[0xFF, 0xFE, b'\n'][..]);
        let line = reader.read_line().unwrap().unwrap();
        assert!(line.as_str().is_err());
    }

    #[test]
    fn empty_input_yields_no_lines() {
        let mut reader = LineReader::new(&b""[..]);
        assert!(reader.read_line().unwrap().is_none());
    }
}
