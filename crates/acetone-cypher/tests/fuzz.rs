//! Parser fuzzing: the front end must never panic, whatever the input.
//! Stack safety on adversarial nesting is covered separately by the
//! depth-limit unit tests (parser::tests::depth_limit_*); these properties
//! sweep the input space more broadly.

use acetone_cypher::parse;
use proptest::prelude::*;

proptest! {
    /// Arbitrary unicode strings: parse returns Ok or Err, never panics.
    #[test]
    fn arbitrary_input_never_panics(input in "\\PC*") {
        let _ = parse(&input);
    }

    /// Byte-level noise, including invalid UTF-8 lossily converted:
    /// exercises the lexer's byte arithmetic on hostile boundaries.
    #[test]
    fn byte_noise_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let input = String::from_utf8_lossy(&bytes);
        let _ = parse(&input);
    }

    /// Mutation fuzzing: truncate, splice and duplicate slices of valid
    /// corpus queries. Every outcome must be a clean Ok/Err with an
    /// in-bounds span.
    #[test]
    fn mutated_corpus_never_panics(
        seed in 0usize..6,
        cut_a in 0usize..120,
        cut_b in 0usize..120,
    ) {
        const CORPUS: &[&str] = &[
            "MATCH (h:Host {hostname: 'web-01'}) RETURN h.hostname, h.os AS os",
            "MATCH (v:Supplier)<-[:S]-(s) WITH v, count(s) AS n WHERE n > 3 RETURN v, n",
            "RETURN [x IN range(1, 10) WHERE x % 2 = 0 | x * x] AS evens",
            "MATCH (n:Host) AT 'main~5' WHERE (n)-[:RUNS]->(:S) RETURN n",
            "CALL acetone.diff('a', 'b') YIELD kind WHERE kind = 'x' RETURN kind",
            "MATCH (a)-[:R*1..3]->(b) RETURN a.name, CASE WHEN a.x THEN 1 ELSE 2 END",
        ];
        let base = CORPUS[seed % CORPUS.len()];
        // Clamp cut points to char boundaries.
        let clamp = |mut at: usize| {
            at = at.min(base.len());
            while !base.is_char_boundary(at) {
                at -= 1;
            }
            at
        };
        let (a, b) = (clamp(cut_a.min(cut_b)), clamp(cut_a.max(cut_b)));
        for mutated in [
            &base[..a],                                  // truncation
            &format!("{}{}", &base[..a], &base[b..]),    // deletion
            &format!("{}{}{}", base, " ", &base[a..b]),  // duplication
            &format!("{}{}{}", &base[..b], &base[a..b], &base[b..]), // splice
        ] {
            match parse(mutated) {
                Ok(_) => {}
                Err(e) => prop_assert!(e.span().end <= mutated.len()),
            }
        }
    }
}
