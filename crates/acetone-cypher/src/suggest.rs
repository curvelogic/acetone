//! Near-match suggestions for "unknown X" diagnostics ("did you mean …?").
//!
//! A small, dependency-free case-insensitive edit distance (Damerau–
//! Levenshtein restricted to adjacent transpositions, a.k.a. optimal string
//! alignment). The binder uses [`nearest`] to point a typo at the closest
//! declared name; the candidate it returns is escaped by the caller before it
//! reaches a terminal, since candidates come from the schema catalogue, which
//! a hostile clone can write.

/// Optimal string alignment distance between `a` and `b`, compared
/// case-insensitively. Counts single-character insertions, deletions,
/// substitutions and adjacent transpositions each as one edit.
///
/// This is the "restricted" Damerau–Levenshtein: it does not allow editing a
/// substring twice, which is ample for typo detection and keeps the recurrence
/// to three rolling rows.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().flat_map(char::to_lowercase).collect();
    let b: Vec<char> = b.chars().flat_map(char::to_lowercase).collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    // Three rolling rows of the DP matrix: row i-2, i-1 and i.
    let mut prev2 = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (prev[j] + 1) // deletion
                .min(curr[j - 1] + 1) // insertion
                .min(prev[j - 1] + cost); // substitution
            // Adjacent transposition (…xy… vs …yx…).
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(prev2[j - 2] + 1);
            }
            curr[j] = best;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// The closest candidate to `target`, or `None` if nothing is close enough.
///
/// "Close enough" is an edit distance within `max(1, len/3)` where `len` is
/// `target`'s character count, so short names admit one edit and longer names
/// admit proportionally more. Comparison is case-insensitive. Ties break to
/// the smaller distance, then lexicographically, so the result is
/// deterministic regardless of candidate iteration order.
pub fn nearest<'a>(target: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let threshold = (target.chars().count() / 3).max(1);
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let d = distance(target, candidate);
        if d > threshold {
            continue;
        }
        let better = match best {
            None => true,
            Some((bd, bc)) => d < bd || (d == bd && candidate < bc),
        };
        if better {
            best = Some((d, candidate));
        }
    }
    best.map(|(_, candidate)| candidate.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_single_edits() {
        assert_eq!(distance("host", "host"), 0);
        assert_eq!(distance("host", "hosts"), 1); // insertion
        assert_eq!(distance("host", "hos"), 1); // deletion
        assert_eq!(distance("host", "hast"), 1); // substitution
        assert_eq!(distance("host", "hsot"), 1); // adjacent transposition
        assert_eq!(distance("host", "ohst"), 1); // transposition at the front
    }

    #[test]
    fn distance_is_case_insensitive() {
        assert_eq!(distance("Host", "host"), 0);
        assert_eq!(distance("toUpper", "TOUPPER"), 0);
    }

    #[test]
    fn distance_handles_empty_strings() {
        assert_eq!(distance("", ""), 0);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("abc", ""), 3);
    }

    #[test]
    fn nearest_suggests_a_close_typo() {
        assert_eq!(
            nearest("Hsot", ["Host", "Topic"].into_iter()),
            Some("Host".to_owned())
        );
        // A transposed function name within threshold.
        assert_eq!(
            nearest("toUppr", ["toUpper", "toLower"].into_iter()),
            Some("toUpper".to_owned())
        );
        // Case-only difference is the closest possible non-identical match.
        assert_eq!(
            nearest("host", ["Host"].into_iter()),
            Some("Host".to_owned())
        );
    }

    #[test]
    fn nearest_rejects_nonsense() {
        assert_eq!(nearest("Zzzzz", ["Host", "Topic"].into_iter()), None);
        assert_eq!(
            nearest("completely_different", ["abs", "ceil"].into_iter()),
            None
        );
        assert_eq!(nearest("Hsot", std::iter::empty()), None);
    }

    #[test]
    fn nearest_is_deterministic_across_ties() {
        // Two candidates equidistant (distance 1) from the target: the
        // lexicographically smaller wins regardless of iteration order.
        let forward = nearest("ba", ["aa", "ca"].into_iter());
        let backward = nearest("ba", ["ca", "aa"].into_iter());
        assert_eq!(forward, Some("aa".to_owned()));
        assert_eq!(backward, Some("aa".to_owned()));
    }

    #[test]
    fn nearest_respects_the_proportional_threshold() {
        // len 3 -> threshold max(1, 1) = 1: two edits is too far.
        assert_eq!(nearest("abc", ["axy"].into_iter()), None);
        // len 6 -> threshold max(1, 2) = 2: two edits is within range.
        assert_eq!(
            nearest("abcdef", ["abcxyf"].into_iter()),
            Some("abcxyf".to_owned())
        );
    }
}
