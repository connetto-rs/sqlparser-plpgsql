//! Helpers the integration tests share.

use sqlparser::tokenizer::Span;

/// The text `body` writes from the start of `span` up to its end.
pub fn quoted(body: &str, span: Span) -> &str {
    let offset = |line: u64, column: u64| {
        let line = usize::try_from(line).expect("a line fits") - 1;
        let column = usize::try_from(column).expect("a column fits") - 1;
        let start: usize = body.split_inclusive('\n').take(line).map(str::len).sum();
        start
            + body[start..]
                .chars()
                .take(column)
                .map(char::len_utf8)
                .sum::<usize>()
    };
    &body[offset(span.start.line, span.start.column)..offset(span.end.line, span.end.column)]
}
