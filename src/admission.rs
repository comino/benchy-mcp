//! REQ-12: nothing may let the model do unbounded work to somebody else's database.
//!
//! REQ-7 bounds what a query costs *the bench*. None of that bounds what a load costs the system
//! it reads **from**. A row cap is a cap, not a budget: it stops one load being enormous and does
//! nothing about the same large load issued again and again.
//!
//! What is enforced today:
//!
//! | | |
//! |---|---|
//! | **estimate before fetch** | a connector that implements `estimate` is asked first and refused over budget before it fetches — the narrowing is named, the table is never truncated |
//! | **the budget bounds the read** | a fetch's row and byte caps are the smaller of the source's own and what the pass has left, so a connector that does not estimate is bounded too |
//! | **the operator sets it** | 50,000,000 rows and 2 GiB unless the config's `[budget]` says otherwise; a host's session has the operator's (`--session-rows`, `--session-bytes`, or `[budget]` in `host --config`), which its host may lower in `hello` |
//! | **stored reload** | a rebuilt bench re-attaches what the session stored, so a wedged planner costs the engine rather than another fetch from the source |

use crate::exec::Fault;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a whole load pass may cost the systems it reads from.
///
/// The supervisor holds one for the whole session, and a rebuild re-attaches what was stored
/// rather than fetching again. That is what makes it a session budget.
#[derive(Debug)]
pub struct Budget {
    pub max_rows: u64,
    pub max_bytes: u64,
    used_rows: AtomicU64,
    used_bytes: AtomicU64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_rows: 50_000_000,
            max_bytes: 2 * 1024 * 1024 * 1024,
            used_rows: AtomicU64::new(0),
            used_bytes: AtomicU64::new(0),
        }
    }
}

impl Budget {
    /// A budget with its ceilings set and its tally at zero. The tally is private on purpose:
    /// spending is the only way it moves.
    pub fn new(max_rows: u64, max_bytes: u64) -> Self {
        Self {
            max_rows,
            max_bytes,
            ..Self::default()
        }
    }

    pub fn remaining(&self) -> (u64, u64) {
        (
            self.max_rows
                .saturating_sub(self.used_rows.load(Ordering::Relaxed)),
            self.max_bytes
                .saturating_sub(self.used_bytes.load(Ordering::Relaxed)),
        )
    }

    /// What has been spent: what a loader reports to the session's budget, which is not this one.
    pub fn spent(&self) -> (u64, u64) {
        (
            self.used_rows.load(Ordering::Relaxed),
            self.used_bytes.load(Ordering::Relaxed),
        )
    }

    pub fn spend(&self, rows: u64, bytes: u64) {
        self.used_rows.fetch_add(rows, Ordering::Relaxed);
        self.used_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Check a connector's own estimate against what is left.
    ///
    /// Refusing here is the whole point: it happens **before** the fetch, so the upstream system
    /// never does the work. The refusal says what was estimated and what was left, for the
    /// operator, who decides what a source fetches.
    pub fn admit(&self, source: &str, rows: u64, bytes: u64) -> Result<(), Fault> {
        let (rows_left, bytes_left) = self.remaining();
        if rows > rows_left || bytes > bytes_left {
            return Err(Fault::host(
                "E_TOO_LARGE",
                format!(
                    "source `{source}` estimates {rows} row(s) / {bytes} byte(s); this session \
                     has {rows_left} row(s) / {bytes_left} byte(s) of source budget left. \
                     Refused before fetching, so the upstream system did no work."
                ),
            ));
        }
        Ok(())
    }
}

/// What a host's session may hold (docs/HOST-PROTOCOL.md, `hello`): one attachment's rows and
/// bytes, the tables at once, and the session's budget. The operator sets them, above the
/// defaults or below, on the command line or in a config; a host may lower them for its session
/// in `hello`, never raise them, as it lowers the caps of a query
/// (`limits_are_lowered_by_the_host_and_raised_only_by_the_operator`).
///
/// A real host's load reaches 240 MB of Arrow, close to the default 256 MiB an attachment may
/// be: the default is a guard against a mistake, and the operator, who knows the loads and the
/// machine, is the one to move it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLimits {
    pub attach_rows: u64,
    pub attach_bytes: u64,
    pub attachments: u64,
    pub session_rows: u64,
    pub session_bytes: u64,
    /// DuckDB's memory budget in the engine, in bytes (`--memory`): reported, and the least the
    /// ceiling may be lowered to. Not a limit a host may lower.
    pub memory: u64,
    /// The most resident memory the engine may hold, in bytes (`supervisor::Memory`): past it the
    /// engine is killed and rebuilt.
    pub memory_ceiling: u64,
    /// The most resident memory one loader may hold, in bytes, with the connector it runs and
    /// everything under it: past it the load is stopped and nothing of it attached
    /// (`supervisor::LOADER_MEMORY_CEILING`). A host may lower it from protocol 6.
    pub loader_memory_ceiling: u64,
}

impl Default for HostLimits {
    fn default() -> Self {
        let one = crate::exec::Limits::default();
        let session = Budget::default();
        Self {
            attach_rows: one.max_rows,
            attach_bytes: one.max_bytes,
            attachments: crate::supervisor::MAX_ATTACHMENTS as u64,
            session_rows: session.max_rows,
            session_bytes: session.max_bytes,
            // `--memory`'s default, 1GB, and the ceiling that follows from it.
            memory: 1_000_000_000,
            memory_ceiling: 1_500_000_000,
            loader_memory_ceiling: crate::supervisor::LOADER_MEMORY_CEILING,
        }
    }
}

impl HostLimits {
    /// Each limit by its name in `hello`.
    pub fn named(&mut self) -> [(&'static str, &mut u64); 7] {
        [
            ("attach_rows", &mut self.attach_rows),
            ("attach_bytes", &mut self.attach_bytes),
            ("attachments", &mut self.attachments),
            ("session_rows", &mut self.session_rows),
            ("session_bytes", &mut self.session_bytes),
            ("memory_ceiling", &mut self.memory_ceiling),
            ("loader_memory_ceiling", &mut self.loader_memory_ceiling),
        ]
    }

    /// These limits, lowered where a host's `hello` asks: never raised, and never to nothing — a
    /// limit of 0 admits nothing, and the first attach would fail far from what caused it.
    pub fn lowered(
        mut self,
        asked: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, String> {
        for (k, v) in asked {
            let mut named = self.named();
            let Some((_, slot)) = named.iter_mut().find(|(n, _)| n == k) else {
                return Err(format!(
                    "`limits` has no field `{k}`; it takes attach_rows, attach_bytes, \
                     attachments, session_rows, session_bytes, memory_ceiling and \
                     loader_memory_ceiling"
                ));
            };
            let Some(n) = v.as_u64() else {
                return Err(format!("`limits.{k}` must be a whole number"));
            };
            if n > **slot {
                return Err(format!(
                    "`limits.{k}` may lower the session's {} but never raise it",
                    **slot
                ));
            }
            if n == 0 {
                return Err(format!("`limits.{k}` must be at least 1"));
            }
            **slot = n;
        }
        // Under DuckDB's own budget, every query DuckDB would refuse cleanly as out of memory
        // would kill the engine instead (`a_host_may_lower_the_memory_ceiling_but_never_raise_it`).
        if self.memory_ceiling < self.memory {
            return Err(format!(
                "`limits.memory_ceiling` must be at least {}, DuckDB's own memory budget",
                self.memory
            ));
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_refuses_before_the_fetch() {
        let b = Budget {
            max_rows: 1000,
            max_bytes: 1_000_000,
            ..Budget::default()
        };
        assert!(b.admit("t", 900, 10).is_ok());
        b.spend(900, 10);
        let f = b.admit("t", 200, 10).unwrap_err();
        assert_eq!(f.code, "E_TOO_LARGE");
        assert!(f.detail.contains("100 row(s)"), "{}", f.detail);
        assert!(
            f.detail.contains("upstream system did no work"),
            "{}",
            f.detail
        );
    }

    #[test]
    fn exactly_what_is_left_fits_and_one_more_does_not() {
        let b = Budget::new(100, 1000);
        b.spend(40, 400);
        assert!(b.admit("t", 60, 600).is_ok(), "exactly what is left fits");
        assert!(b.admit("t", 61, 600).is_err(), "one row more does not");
        assert!(b.admit("t", 60, 601).is_err(), "nor one byte more");
    }

    #[test]
    fn spending_past_the_budget_saturates_rather_than_wrapping() {
        let b = Budget {
            max_rows: 10,
            ..Budget::default()
        };
        b.spend(999, 0);
        assert_eq!(b.remaining().0, 0);
        assert!(b.admit("t", 1, 0).is_err());
    }
}
