//! Source spans: byte ranges into the original query text.

/// A half-open byte range `[start, end)` into the query source. Every AST
/// node and every error carries one, so diagnostics and later phases
/// (binder, planner) can always point back at the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    /// The span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// 1-based (line, column) of the span start, counting columns in
    /// characters. For rendering diagnostics.
    pub fn line_col(&self, source: &str) -> (usize, usize) {
        let upto = &source[..self.start.min(source.len())];
        let line = upto.matches('\n').count() + 1;
        let col = upto.chars().rev().take_while(|c| *c != '\n').count() + 1;
        (line, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_covers_both_spans() {
        assert_eq!(Span::new(3, 5).to(Span::new(10, 12)), Span::new(3, 12));
        assert_eq!(Span::new(10, 12).to(Span::new(3, 5)), Span::new(3, 12));
    }

    #[test]
    fn line_col_is_one_based() {
        let src = "RETURN 1\nRETURN 2";
        assert_eq!(Span::new(0, 6).line_col(src), (1, 1));
        assert_eq!(Span::new(9, 15).line_col(src), (2, 1));
        assert_eq!(Span::new(16, 17).line_col(src), (2, 8));
    }

    #[test]
    fn line_col_clamps_out_of_range_start() {
        assert_eq!(Span::new(99, 100).line_col("hi"), (1, 3));
    }
}
