//! End-to-end tests that run the built `jsonl-peek` binary against files on
//! disk, exercising the argument parsing and I/O plumbing in `main.rs` that
//! the library's unit tests never touch.

use std::path::PathBuf;
use std::process::{Command, Output};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jsonl-peek"))
        .args(args)
        .output()
        .expect("failed to run jsonl-peek")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout was not UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr was not UTF-8")
}

#[test]
fn head_reads_the_first_n_lines_from_a_file() {
    let path = fixture("basic.jsonl");
    let output = run(&["head", "-n", "2", path.to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "{\"id\":1,\"role\":\"user\",\"tags\":[\"a\",\"b\"]}\n{\"id\":2,\"role\":\"assistant\"}\n"
    );
}

#[test]
fn head_on_a_missing_file_exits_with_an_error() {
    let path = fixture("does-not-exist.jsonl");
    let output = run(&["head", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("does-not-exist.jsonl"));
}

#[test]
fn stats_json_reports_line_and_validity_counts() {
    let path = fixture("basic.jsonl");
    let output = run(&["stats", "--json", path.to_str().unwrap()]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("\"lines\":5"));
    assert!(text.contains("\"blank\":1"));
    assert!(text.contains("\"valid\":3"));
    assert!(text.contains("\"invalid\":1"));
}

#[test]
fn schema_json_discovers_paths_across_the_file() {
    let path = fixture("basic.jsonl");
    let output = run(&["schema", "--json", path.to_str().unwrap()]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("\"records\":3"));
    assert!(text.contains("\"path\":\"tags\""));
    assert!(text.contains("\"path\":\"tags[]\""));
}

#[test]
fn sample_with_a_fixed_seed_is_deterministic() {
    let path = fixture("basic.jsonl");
    let first = run(&["sample", "-n", "2", "--seed", "42", path.to_str().unwrap()]);
    let second = run(&["sample", "-n", "2", "--seed", "42", path.to_str().unwrap()]);
    assert!(first.status.success());
    assert!(second.status.success());
    assert_eq!(stdout(&first), stdout(&second));
    assert_eq!(stdout(&first).lines().count(), 2);
}

#[test]
fn sample_never_returns_blank_lines() {
    let path = fixture("basic.jsonl");
    let output = run(&["sample", "-n", "10", "--seed", "1", path.to_str().unwrap()]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(!text.lines().any(str::is_empty));
    assert_eq!(text.lines().count(), 4);
}

#[test]
fn unknown_command_exits_with_a_usage_error() {
    let output = run(&["bogus"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("unknown command"));
}
