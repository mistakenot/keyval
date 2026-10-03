//! The telemetry port's events (PLAN.md §7.2): structured log records ([`LogEvent`],
//! O-3) and observations for the canonical trace and the checkers ([`Observation`],
//! §10.2).
//!
//! These are the core's own types. The observation **schema** that oracles read is a
//! separate, documented format (T-12); the trace recorder translates. W14 and W36 add the
//! variants their events need. CD-5 then forces every consumer to handle them.

use crate::client::AttemptId;
use crate::consensus::Role;
use crate::ids::NodeId;
use crate::lifecycle::Lifecycle;
use crate::log_index::{LogIndex, Term};

/// How serious a log record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// Normal operation: role and lifecycle changes.
    Info,
    /// Something was refused or dropped within its bounds.
    Warn,
    /// The node cannot continue as it was (it moves to `failed`).
    Error,
}

/// An input dropped by a bound (§7.11), counted and logged, never silent (CD-9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DroppedInput {
    /// A peer message: the controller queue was full, or the envelope was rejected.
    PeerMessage,
}

/// A structured log record (O-3: role changes, elections, recovery and errors).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogEvent {
    /// The node's Raft role changed.
    RoleChanged {
        /// The new role.
        role: Role,
        /// The term it holds the role in.
        term: Term,
        /// The leader it now believes in, if any.
        leader: Option<NodeId>,
    },
    /// The node's lifecycle changed (§7.7).
    LifecycleChanged {
        /// The previous state.
        from: Lifecycle,
        /// The new state.
        to: Lifecycle,
    },
    /// An input was dropped by a bound.
    Dropped {
        /// What was dropped.
        input: DroppedInput,
    },
}

impl LogEvent {
    /// The record's level.
    #[must_use]
    pub const fn level(self) -> LogLevel {
        match self {
            Self::RoleChanged { .. } => LogLevel::Info,
            Self::LifecycleChanged { to, .. } => match to {
                Lifecycle::Failed => LogLevel::Error,
                Lifecycle::Starting | Lifecycle::Recovering | Lifecycle::Ready => LogLevel::Info,
            },
            Self::Dropped { .. } => LogLevel::Warn,
        }
    }
}

/// An observation for the canonical trace and the checkers (§10.2, §10.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    /// The published view advanced to `index` (C-3, C-4).
    Applied {
        /// The new applied index.
        index: LogIndex,
    },
    /// An attempt was answered.
    Responded {
        /// The attempt.
        attempt: AttemptId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_severity() {
        let failed = LogEvent::LifecycleChanged {
            from: Lifecycle::Ready,
            to: Lifecycle::Failed,
        };
        let ready = LogEvent::LifecycleChanged {
            from: Lifecycle::Recovering,
            to: Lifecycle::Ready,
        };
        let dropped = LogEvent::Dropped {
            input: DroppedInput::PeerMessage,
        };
        let elected = LogEvent::RoleChanged {
            role: Role::Leader,
            term: Term::new(2).unwrap(),
            leader: NodeId::new(1).ok(),
        };
        assert_eq!(
            failed.level(),
            LogLevel::Error,
            "rule: moving to failed is an error (DM-7, §7.11)"
        );
        assert_eq!(
            ready.level(),
            LogLevel::Info,
            "rule: recovery completing is normal (O-3)"
        );
        assert_eq!(
            dropped.level(),
            LogLevel::Warn,
            "rule: drops are counted and visible (CD-9)"
        );
        assert_eq!(
            elected.level(),
            LogLevel::Info,
            "rule: elections are logged (O-3)"
        );
    }
}
