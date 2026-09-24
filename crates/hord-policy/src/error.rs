//! Policy parse errors.

use std::fmt;
use std::ops::Range;

use serde::Serialize;

/// A 1-based position in the policy source.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Location {
    /// Line, counting from 1.
    pub line: usize,
    /// Column in characters, counting from 1.
    pub column: usize,
}

impl Location {
    /// The position of byte `offset` in `source`. Offsets past the end
    /// clamp to the end.
    #[must_use]
    pub fn of(source: &str, offset: usize) -> Self {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let before = &source[..offset];
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = before[line_start..].chars().count() + 1;
        Self { line, column }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// A policy that is not valid TOML, does not have the §7.2 shape, or has an
/// invalid value (an unknown actor, a bad glob, a malformed requirement).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, thiserror::Error)]
pub struct ParseError {
    /// What is wrong.
    pub message: String,
    /// Where, when the error comes from source text.
    pub location: Option<Location>,
    /// Byte range in the source, when known.
    pub span: Option<Range<usize>>,
}

impl ParseError {
    pub(crate) fn at(source: Option<&str>, span: Option<Range<usize>>, message: String) -> Self {
        let location = match (source, &span) {
            (Some(source), Some(span)) => Some(Location::of(source, span.start)),
            _ => None,
        };
        Self {
            message,
            location,
            span: span.filter(|_| source.is_some()),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.location {
            Some(location) => write!(f, "{location}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}
