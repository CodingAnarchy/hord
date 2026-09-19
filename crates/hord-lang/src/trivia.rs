//! Trivia attachment (spec §3.3, DECIDED).
//!
//! Leading trivia (comments, blank lines, whitespace that is not on the same
//! line as the preceding token) attaches to the **following** token. Trailing
//! trivia on the **same line** attaches to the **preceding** token.
//!
//! Adapters classify tokens as [`TokenSpan`]s (byte ranges in the source);
//! this module only attaches. [`Lexeme`] is the owned form used by tests.
//! Tree-sitter lives in language crates, not here.

use std::ops::Range;

use hord_core::{Bytes, NodeKind};

/// One classified piece of a source file, before trivia attachment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Lexeme {
    /// A non-trivia token.
    Token {
        /// Adapter-defined token kind.
        kind: NodeKind,
        /// Token bytes with no trivia.
        text: Bytes,
    },
    /// Comments, whitespace, or blank lines.
    Trivia {
        /// Exact trivia bytes.
        text: Bytes,
    },
}

impl Lexeme {
    /// A token lexeme.
    #[must_use]
    pub fn token(kind: impl Into<NodeKind>, text: impl Into<Bytes>) -> Self {
        Self::Token {
            kind: kind.into(),
            text: text.into(),
        }
    }

    /// A trivia lexeme.
    #[must_use]
    pub fn trivia(text: impl Into<Bytes>) -> Self {
        Self::Trivia { text: text.into() }
    }
}

/// A token's kind and byte range in the source, before trivia attachment.
///
/// `start..end` is the trivia-stripped token. Gaps between spans are trivia.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenSpan {
    /// Adapter-defined token kind.
    pub kind: NodeKind,
    /// Byte offset of the token start in the source.
    pub start: usize,
    /// Byte offset of the token end in the source (exclusive).
    pub end: usize,
}

impl TokenSpan {
    /// A token covering `start..end` in the source.
    #[must_use]
    pub fn new(kind: impl Into<NodeKind>, start: usize, end: usize) -> Self {
        Self {
            kind: kind.into(),
            start,
            end,
        }
    }
}

/// A token with trivia attached as ranges into the source (spec §3.3).
///
/// `source[raw]` is leading+text+trailing and is contiguous. `source[text]`
/// is the trivia-stripped token used by [`crate::normalized_hash`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachedSpan {
    /// Adapter-defined token kind.
    pub kind: NodeKind,
    /// Inclusive-exclusive range of leading+text+trailing in the source.
    pub raw: Range<usize>,
    /// Inclusive-exclusive range of the trivia-stripped token in the source.
    pub text: Range<usize>,
}

/// A token with trivia attached per spec §3.3 (owned bytes).
///
/// `raw == leading || text || trailing`. [`text`](Self::text) is the
/// trivia-stripped form used by [`crate::normalized_hash`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachedToken {
    /// Adapter-defined token kind.
    pub kind: NodeKind,
    /// Trivia attached as leading (following-token rule).
    pub leading: Bytes,
    /// Token bytes with no trivia.
    pub text: Bytes,
    /// Trivia attached as trailing (same-line rule, plus EOF remainder).
    pub trailing: Bytes,
}

impl AttachedToken {
    /// Concatenation of leading, text, and trailing bytes.
    #[must_use]
    pub fn raw(&self) -> Bytes {
        let mut raw =
            Vec::with_capacity(self.leading.len() + self.text.len() + self.trailing.len());
        raw.extend_from_slice(&self.leading);
        raw.extend_from_slice(&self.text);
        raw.extend_from_slice(&self.trailing);
        Bytes::new(raw)
    }
}

/// Attach trivia in `source` to `tokens` (spec §3.3).
///
/// Between two tokens, bytes up to but not including the first `\n` are
/// trailing on the preceding token; the rest (starting at `\n`, if any) is
/// leading on the following token. Trivia before the first token is leading.
/// Trivia after the last token — including multiline remainder — is trailing
/// on that token so the attachment is lossless. An all-trivia input (no
/// tokens) yields an empty vec.
///
/// Token spans must be in source order, non-overlapping, and within `source`.
/// Concatenation of each span's `raw` slice equals `source`.
#[must_use]
pub fn attach_trivia_spans(source: &[u8], tokens: Vec<TokenSpan>) -> Vec<AttachedSpan> {
    let n = tokens.len();
    let mut spans: Vec<AttachedSpan> = tokens
        .into_iter()
        .map(|t| AttachedSpan {
            kind: t.kind,
            raw: t.start..t.end,
            text: t.start..t.end,
        })
        .collect();
    if n == 0 {
        return spans;
    }
    for i in 0..n {
        let gap_from = if i == 0 { 0 } else { spans[i - 1].text.end };
        let tok_start = spans[i].text.start;
        let lead_start = if i == 0 {
            0
        } else {
            match source
                .get(gap_from..tok_start)
                .and_then(|gap| gap.iter().position(|&b| b == b'\n'))
            {
                Some(rel) => gap_from + rel,
                None => tok_start,
            }
        };
        spans[i].raw.start = lead_start;
        if i > 0 {
            spans[i - 1].raw.end = lead_start;
        }
    }
    spans[n - 1].raw.end = source.len();
    spans
}

/// Attach trivia in `lexemes` to tokens (spec §3.3).
///
/// Builds a scratch source and delegates to [`attach_trivia_spans`]. Prefer
/// that function on the parse path so token bytes are not copied first.
#[must_use]
pub fn attach_trivia(lexemes: &[Lexeme]) -> Vec<AttachedToken> {
    let mut source = Vec::new();
    let mut tokens = Vec::new();
    for lexeme in lexemes {
        match lexeme {
            Lexeme::Trivia { text } => source.extend_from_slice(text.as_slice()),
            Lexeme::Token { kind, text } => {
                let start = source.len();
                source.extend_from_slice(text.as_slice());
                tokens.push(TokenSpan {
                    kind: *kind,
                    start,
                    end: source.len(),
                });
            }
        }
    }
    attach_trivia_spans(&source, tokens)
        .into_iter()
        .map(|span| AttachedToken {
            kind: span.kind,
            leading: Bytes::from(&source[span.raw.start..span.text.start]),
            text: Bytes::from(&source[span.text.start..span.text.end]),
            trailing: Bytes::from(&source[span.text.end..span.raw.end]),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(kind: &str, text: &str) -> Lexeme {
        Lexeme::token(kind, text.as_bytes())
    }

    fn triv(text: &str) -> Lexeme {
        Lexeme::trivia(text.as_bytes())
    }

    fn bytes(s: &str) -> Bytes {
        Bytes::from(s.as_bytes())
    }

    #[test]
    fn leading_trivia_attaches_to_following_token() {
        let tokens = attach_trivia(&[
            triv("// doc\n"),
            tok("fn_kw", "fn"),
            triv(" "),
            tok("ident", "foo"),
        ]);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].leading, bytes("// doc\n"));
        assert_eq!(tokens[0].text, bytes("fn"));
        assert_eq!(tokens[0].trailing, bytes(" "));
        assert_eq!(tokens[1].leading, Bytes::default());
        assert_eq!(tokens[1].text, bytes("foo"));
        assert_eq!(tokens[1].trailing, Bytes::default());
    }

    #[test]
    fn same_line_trailing_attaches_to_preceding_token() {
        let tokens = attach_trivia(&[
            tok("ident", "foo"),
            triv("  // trailing"),
            triv("\n"),
            tok("ident", "bar"),
        ]);
        assert_eq!(tokens[0].trailing, bytes("  // trailing"));
        assert_eq!(tokens[1].leading, bytes("\n"));
        assert_eq!(tokens[1].text, bytes("bar"));
    }

    #[test]
    fn blank_lines_are_leading_on_the_next_token() {
        let tokens = attach_trivia(&[tok("ident", "foo"), triv("\n\n    "), tok("ident", "bar")]);
        assert_eq!(tokens[0].trailing, Bytes::default());
        assert_eq!(tokens[1].leading, bytes("\n\n    "));
    }

    #[test]
    fn same_line_spaces_are_trailing_not_leading() {
        let tokens = attach_trivia(&[tok("ident", "foo"), triv("  "), tok("ident", "bar")]);
        assert_eq!(tokens[0].trailing, bytes("  "));
        assert_eq!(tokens[1].leading, Bytes::default());
    }

    #[test]
    fn eof_trivia_attaches_to_the_last_token() {
        let tokens = attach_trivia(&[tok("ident", "foo"), triv("\n")]);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].trailing, bytes("\n"));
        assert_eq!(tokens[0].raw(), bytes("foo\n"));
    }

    #[test]
    fn raw_is_leading_text_trailing() {
        let tokens = attach_trivia(&[triv("  "), tok("ident", "x"), triv(" // c")]);
        assert_eq!(tokens[0].raw(), bytes("  x // c"));
        assert_eq!(tokens[0].text, bytes("x"));
    }

    #[test]
    fn all_trivia_yields_no_tokens() {
        let tokens = attach_trivia(&[triv("// only\n")]);
        assert!(tokens.is_empty());
    }

    #[test]
    fn concatenated_trivia_pieces_split_once() {
        let tokens = attach_trivia(&[
            tok("ident", "a"),
            triv(" "),
            triv("// c"),
            triv("\n  "),
            tok("ident", "b"),
        ]);
        assert_eq!(tokens[0].trailing, bytes(" // c"));
        assert_eq!(tokens[1].leading, bytes("\n  "));
    }

    #[test]
    fn spans_concat_equals_source() {
        let source = b"// d\nfn  foo\n";
        let tokens = vec![TokenSpan::new("kw", 5, 7), TokenSpan::new("ident", 9, 12)];
        let spans = attach_trivia_spans(source, tokens);
        assert_eq!(spans.len(), 2);
        let mut concat = Vec::new();
        for span in &spans {
            concat.extend_from_slice(&source[span.raw.start..span.raw.end]);
        }
        assert_eq!(concat.as_slice(), source.as_slice());
        assert_eq!(&source[spans[0].text.start..spans[0].text.end], b"fn");
        assert_eq!(&source[spans[0].raw.start..spans[0].text.start], b"// d\n");
        assert_eq!(&source[spans[0].text.end..spans[0].raw.end], b"  ");
        assert_eq!(&source[spans[1].text.start..spans[1].text.end], b"foo");
        assert_eq!(&source[spans[1].text.end..spans[1].raw.end], b"\n");
    }
}
