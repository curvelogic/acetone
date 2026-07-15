//! The query resource governor (ADR-0036, bead acetone-iq6).
//!
//! The read/write executor is a materialised clause pipeline, so a single
//! untrusted query can drive unbounded CPU or memory — an unbounded
//! variable-length `MATCH (a)-[*]->(b)` over a dense graph, or a huge
//! `range(0, N)`. The governor bounds that with a **deterministic work
//! budget** as the canonical cap: the same query over the same graph
//! charges the same work and yields the same success or error on every
//! machine, which is what makes the caps property-testable and lets the
//! frozen 0.2 library API promise reproducible behaviour. Wall-clock time is
//! an optional, off-by-default backstop for the library-embedding case.
//!
//! One [`Governor`] is constructed per query at the single execution funnel
//! and threaded into every [`crate::exec::eval::EvalCtx`] by shared
//! reference. Its counters are interior-mutable ([`Cell`]) because the
//! executor is single-threaded and `EvalCtx` is rebuilt per clause — the
//! budget must outlive each context, so it cannot be a value field.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::exec::eval::{ExecError, ResourceLimit};
use crate::span::Span;

/// The public, governed configuration surface (frozen at the 0.2 gate). Every
/// field is an inclusive cap: a query may reach it but not exceed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryLimits {
    /// Canonical deterministic odometer: total charged work across the query
    /// (one unit per produced row, per expansion hop, per collection cell).
    /// This is the backstop that no other cap can slip past.
    pub max_work_units: u64,
    /// The largest a single materialised result-row set may grow.
    pub max_result_rows: u64,
    /// Cumulative variable-length / expansion hops over the whole query.
    pub max_expansion_steps: u64,
    /// The largest a single list/collection (e.g. `range()`) may be.
    pub max_collection_len: u64,
    /// Optional wall-clock backstop. `None` (the default) keeps the default
    /// execution path deterministic and free of clock reads.
    pub wall_clock: Option<Duration>,
}

impl Default for QueryLimits {
    /// Generous defaults: realistic registry/lab-graph queries run orders of
    /// magnitude under them, while the known pathologies trip in well under a
    /// second. Validated by the property/fuzz regime, not asserted here.
    fn default() -> Self {
        QueryLimits {
            max_work_units: 100_000_000,
            max_result_rows: 1_000_000,
            max_expansion_steps: 1_000_000,
            max_collection_len: 10_000_000,
            wall_clock: None,
        }
    }
}

impl QueryLimits {
    /// Effectively-unbounded limits, for callers and tests that deliberately
    /// opt out of governing (e.g. an operator running their own trusted query
    /// who wants no ceiling). Still deterministic — no wall clock.
    pub fn unbounded() -> Self {
        QueryLimits {
            max_work_units: u64::MAX,
            max_result_rows: u64::MAX,
            max_expansion_steps: u64::MAX,
            max_collection_len: u64::MAX,
            wall_clock: None,
        }
    }
}

/// Poll the wall clock only once per this many work units, so the backstop
/// (when enabled) costs one `Instant::now()` per stride rather than per charge.
const CLOCK_POLL_STRIDE: u64 = 4096;

/// The per-query budget. Charged at every row/hop/collection growth seam;
/// returns [`ExecError::ResourceExceeded`] the moment a cap is crossed, so the
/// executor stops before it hangs or OOMs.
pub struct Governor {
    limits: QueryLimits,
    work: Cell<u64>,
    expansion: Cell<u64>,
    deadline: Option<Instant>,
    work_at_last_poll: Cell<u64>,
}

impl Governor {
    /// A governor enforcing `limits`. The wall-clock deadline (if any) is
    /// anchored now, at construction — i.e. at the start of the query.
    pub fn new(limits: QueryLimits) -> Self {
        let deadline = limits.wall_clock.map(|budget| Instant::now() + budget);
        Governor {
            limits,
            work: Cell::new(0),
            expansion: Cell::new(0),
            deadline,
            work_at_last_poll: Cell::new(0),
        }
    }

    fn exceeded(limit: ResourceLimit) -> ExecError {
        // Resource exhaustion is a whole-query condition, not tied to one
        // token, so it carries the default (query-start) span; it still
        // renders through the shared error plumbing.
        ExecError::ResourceExceeded {
            limit,
            span: Span::default(),
        }
    }

    /// Charge `n` units onto the canonical odometer and poll the wall-clock
    /// backstop on stride boundaries. Every other charge routes through here,
    /// so `max_work_units` and the deadline bound the query as a whole.
    fn charge_work(&self, n: u64) -> Result<(), ExecError> {
        let work = self.work.get().saturating_add(n);
        self.work.set(work);
        if work > self.limits.max_work_units {
            return Err(Self::exceeded(ResourceLimit::WorkUnits));
        }
        if let Some(deadline) = self.deadline
            && work.wrapping_sub(self.work_at_last_poll.get()) >= CLOCK_POLL_STRIDE
        {
            self.work_at_last_poll.set(work);
            if Instant::now() >= deadline {
                return Err(Self::exceeded(ResourceLimit::WallClock));
            }
        }
        Ok(())
    }

    /// Charge one row about to be pushed into a set that currently holds
    /// `set_len` rows. Errors if that set has reached `max_result_rows`.
    pub fn row(&self, set_len: usize) -> Result<(), ExecError> {
        if set_len as u64 >= self.limits.max_result_rows {
            return Err(Self::exceeded(ResourceLimit::ResultRows));
        }
        self.charge_work(1)
    }

    /// Charge one variable-length / expansion hop (one edge traversal during
    /// pattern matching). Errors past `max_expansion_steps`.
    pub fn hop(&self) -> Result<(), ExecError> {
        let expansion = self.expansion.get().saturating_add(1);
        self.expansion.set(expansion);
        if expansion > self.limits.max_expansion_steps {
            return Err(Self::exceeded(ResourceLimit::ExpansionSteps));
        }
        self.charge_work(1)
    }

    /// Charge building a collection of `len` elements *before* it is
    /// allocated, so an oversized `range()`/list is rejected up front rather
    /// than after exhausting memory. Errors past `max_collection_len`.
    pub fn collection(&self, len: u64) -> Result<(), ExecError> {
        if len > self.limits.max_collection_len {
            return Err(Self::exceeded(ResourceLimit::CollectionLen));
        }
        self.charge_work(len)
    }

    /// Total work charged so far. Deterministic for a given query + graph +
    /// limits; the determinism property test asserts this is reproducible.
    pub fn work_units(&self) -> u64 {
        self.work.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(err: ExecError) -> ResourceLimit {
        match err {
            ExecError::ResourceExceeded { limit, .. } => limit,
            other => panic!("expected ResourceExceeded, got {other:?}"),
        }
    }

    #[test]
    fn row_cap_rejects_the_row_that_would_exceed_the_set_bound() {
        let limits = QueryLimits {
            max_result_rows: 2,
            ..QueryLimits::unbounded()
        };
        let gov = Governor::new(limits);
        // A set may grow up to the cap: pushing into a set of size 0 and 1 is
        // fine, pushing into a set already at 2 is rejected.
        assert!(gov.row(0).is_ok());
        assert!(gov.row(1).is_ok());
        assert_eq!(limit(gov.row(2).unwrap_err()), ResourceLimit::ResultRows);
    }

    #[test]
    fn expansion_cap_bounds_cumulative_hops() {
        let limits = QueryLimits {
            max_expansion_steps: 3,
            ..QueryLimits::unbounded()
        };
        let gov = Governor::new(limits);
        assert!(gov.hop().is_ok());
        assert!(gov.hop().is_ok());
        assert!(gov.hop().is_ok());
        assert_eq!(limit(gov.hop().unwrap_err()), ResourceLimit::ExpansionSteps);
    }

    #[test]
    fn collection_cap_rejects_before_allocating() {
        let limits = QueryLimits {
            max_collection_len: 10,
            ..QueryLimits::unbounded()
        };
        let gov = Governor::new(limits);
        assert!(gov.collection(10).is_ok());
        assert_eq!(
            limit(gov.collection(11).unwrap_err()),
            ResourceLimit::CollectionLen
        );
    }

    #[test]
    fn work_cap_is_the_odometer_over_every_charge() {
        let limits = QueryLimits {
            max_work_units: 5,
            ..QueryLimits::unbounded()
        };
        let gov = Governor::new(limits);
        // Rows, hops and collection cells all charge the same odometer.
        gov.row(0).unwrap(); // +1 -> 1
        gov.hop().unwrap(); // +1 -> 2
        gov.collection(2).unwrap(); // +2 -> 4
        gov.row(0).unwrap(); // +1 -> 5
        assert_eq!(gov.work_units(), 5);
        assert_eq!(limit(gov.hop().unwrap_err()), ResourceLimit::WorkUnits);
    }

    #[test]
    fn work_counting_is_deterministic_across_governors() {
        let run = || {
            let gov = Governor::new(QueryLimits::unbounded());
            for _ in 0..100 {
                gov.hop().unwrap();
                gov.row(0).unwrap();
                gov.collection(3).unwrap();
            }
            gov.work_units()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn unbounded_charges_a_lot_without_erroring() {
        let gov = Governor::new(QueryLimits::unbounded());
        for _ in 0..1_000_000 {
            gov.hop().unwrap();
        }
        assert_eq!(gov.work_units(), 1_000_000);
    }
}
