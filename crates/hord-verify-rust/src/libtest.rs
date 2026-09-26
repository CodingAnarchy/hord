//! Reading libtest's human output (`cargo test`): which tests failed, and
//! the totals.

use std::collections::BTreeSet;

/// What a `cargo test` run reported.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TestReport {
    /// Tests reported `FAILED` (names as libtest prints them).
    pub failed: BTreeSet<String>,
    /// Sum of `passed` over every `test result:` line.
    pub passed: u64,
    /// Sum of `ignored`.
    pub ignored: u64,
    /// Number of `test result:` lines (test binaries that ran).
    pub binaries: u64,
}

impl TestReport {
    /// Parse libtest output (stdout of `cargo test`).
    #[must_use]
    pub fn parse(stdout: &str) -> Self {
        let mut report = Self::default();
        for line in stdout.lines() {
            let line = strip_ansi(line);
            let line = line.as_str();
            if let Some(rest) = line.strip_prefix("test ")
                && let Some(name) = rest
                    .strip_suffix(" - should panic ... FAILED")
                    .or_else(|| rest.strip_suffix(" ... FAILED"))
            {
                report.failed.insert(name.trim().to_owned());
            } else if let Some(rest) = line.strip_prefix("test result: ") {
                report.binaries += 1;
                report.passed += count(rest, "passed");
                report.ignored += count(rest, "ignored");
            }
        }
        report
    }
}

/// The number before `word` in `… 3 passed; 1 failed; …`.
fn count(summary: &str, word: &str) -> u64 {
    summary
        .split(';')
        .filter_map(|part| {
            let mut it = part.split_whitespace().rev();
            let label = it.next()?;
            let value = it.next()?;
            (label == word).then(|| value.parse::<u64>().ok()).flatten()
        })
        .sum()
}

/// Test names from `--list --format terse` output (`name: test` lines).
#[must_use]
pub fn parse_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(strip_ansi)
        .filter_map(|l| l.strip_suffix(": test").map(str::to_owned))
        .collect()
}

/// `text` without ANSI escape sequences (`ESC [ … letter`): cargo and
/// libtest color their output when told to (CI often sets
/// `CARGO_TERM_COLOR=always`), and the parsers read plain text.
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_failures_and_totals() {
        let out = "\nrunning 3 tests\ntest a::b ... ok\ntest a::c ... FAILED\ntest d ... ignored\n\
                   test e - should panic ... FAILED\n\
                   test result: FAILED. 1 passed; 2 failed; 1 ignored; 0 measured; 9 filtered out; finished in 0.1s\n\
                   test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s\n";
        let r = TestReport::parse(out);
        assert_eq!(
            r.failed,
            ["a::c".to_owned(), "e".to_owned()].into_iter().collect()
        );
        assert_eq!((r.passed, r.ignored, r.binaries), (5, 1, 2));
        // Colored output (`--color always`) reads the same.
        let colored = "test a::c ... \u{1b}[31mFAILED\u{1b}[0m\n\
                       test result: \u{1b}[31mFAILED\u{1b}[0m. 1 passed; 1 failed; 0 ignored\n";
        let r = TestReport::parse(colored);
        assert_eq!(r.failed, ["a::c".to_owned()].into_iter().collect());
        assert_eq!((r.passed, r.binaries), (1, 1));
    }

    #[test]
    fn parses_lists() {
        let out = "a::b: test\nc: test\nbench_x: benchmark\n";
        assert_eq!(parse_list(out), vec!["a::b", "c"]);
    }
}
