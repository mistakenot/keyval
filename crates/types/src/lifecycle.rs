//! [`Lifecycle`]: a node incarnation's state (PLAN.md O-1, §7.7, CD-2).

use core::fmt;

/// The lifecycle of one node incarnation (PLAN.md O-1, §7.7).
///
/// Invariants:
/// - it only moves forward: `Starting → Recovering → Ready`, and any state but `Failed`
///   may move to `Failed` ([`Lifecycle::can_become`]);
/// - `Failed` is final for the incarnation; a restart is a new incarnation that begins
///   at `Starting` again (DM-7);
/// - only `Ready` serves reads or admits writes (A-2, §7.9); every other state answers
///   `recovering`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// The process is up but has not begun recovery.
    Starting,
    /// Replaying the journal (§7.7 steps 1–6).
    Recovering,
    /// A complete committed read view is available (§7.7 step 7).
    Ready,
    /// The node stopped appending (journal hard stop or an I/O error, DM-7).
    Failed,
}

/// A lifecycle state in which the node is not ready: the `lifecycle` of a
/// `503 recovering` refusal (the API spec's `NotReadyLifecycle`, O-1).
///
/// Invariant: never `ready`, by its type: it has no such variant, so a refusal can never
/// claim the node is ready (CD-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotReadyLifecycle {
    /// The process is up but has not begun recovery.
    Starting,
    /// Replaying the journal.
    Recovering,
    /// The node stopped appending.
    Failed,
}

impl NotReadyLifecycle {
    /// The same state as a [`Lifecycle`].
    #[must_use]
    pub const fn lifecycle(self) -> Lifecycle {
        match self {
            Self::Starting => Lifecycle::Starting,
            Self::Recovering => Lifecycle::Recovering,
            Self::Failed => Lifecycle::Failed,
        }
    }

    /// The state as the API spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.lifecycle().as_str()
    }
}

impl Lifecycle {
    /// The state as a [`NotReadyLifecycle`], or `None` when `Ready`.
    #[must_use]
    pub const fn not_ready(self) -> Option<NotReadyLifecycle> {
        match self {
            Self::Starting => Some(NotReadyLifecycle::Starting),
            Self::Recovering => Some(NotReadyLifecycle::Recovering),
            Self::Ready => None,
            Self::Failed => Some(NotReadyLifecycle::Failed),
        }
    }
}

impl Lifecycle {
    /// Every state, in lifecycle order.
    pub const ALL: [Self; 4] = [Self::Starting, Self::Recovering, Self::Ready, Self::Failed];

    /// Whether moving from `self` to `next` is allowed. Every pair is spelled out, so a new
    /// state must decide each of its transitions here (CD-5).
    #[must_use]
    pub const fn can_become(self, next: Self) -> bool {
        match (self, next) {
            (Self::Starting, Self::Recovering)
            | (Self::Recovering, Self::Ready)
            | (Self::Starting | Self::Recovering | Self::Ready, Self::Failed) => true,
            (Self::Starting, Self::Starting | Self::Ready)
            | (Self::Recovering, Self::Starting | Self::Recovering)
            | (Self::Ready, Self::Starting | Self::Recovering | Self::Ready)
            | (Self::Failed, Self::Starting | Self::Recovering | Self::Ready | Self::Failed) => {
                false
            }
        }
    }

    /// Whether the node serves reads and admits writes in this state (A-2).
    #[must_use]
    pub const fn is_ready(self) -> bool {
        match self {
            Self::Ready => true,
            Self::Starting | Self::Recovering | Self::Failed => false,
        }
    }

    /// The state as the API spells it (`/v1/health/live`, O-1).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Recovering => "recovering",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for Lifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    // check: DOMAIN-ENUMS

    use super::*;

    #[test]
    fn a_not_ready_lifecycle_is_every_state_but_ready() {
        let observed: Vec<(Lifecycle, Option<&str>)> = Lifecycle::ALL
            .into_iter()
            .map(|state| (state, state.not_ready().map(NotReadyLifecycle::as_str)))
            .collect();
        assert_eq!(
            observed,
            [
                (Lifecycle::Starting, Some("starting")),
                (Lifecycle::Recovering, Some("recovering")),
                (Lifecycle::Ready, None),
                (Lifecycle::Failed, Some("failed")),
            ],
            "rule: NotReadyLifecycle is [starting, recovering, failed] (api/openapi.yaml)"
        );
        for state in Lifecycle::ALL {
            if let Some(not_ready) = state.not_ready() {
                assert_eq!(
                    not_ready.lifecycle(),
                    state,
                    "rule: the conversion is exact"
                );
            }
        }
    }

    #[test]
    fn transitions_only_move_forward_and_failed_is_final() {
        let allowed = [
            (Lifecycle::Starting, Lifecycle::Recovering),
            (Lifecycle::Recovering, Lifecycle::Ready),
            (Lifecycle::Starting, Lifecycle::Failed),
            (Lifecycle::Recovering, Lifecycle::Failed),
            (Lifecycle::Ready, Lifecycle::Failed),
        ];
        for from in Lifecycle::ALL {
            for to in Lifecycle::ALL {
                assert_eq!(
                    from.can_become(to),
                    allowed.contains(&(from, to)),
                    "rule: Starting→Recovering→Ready, any non-failed state →Failed, nothing else (§7.7, DM-7); \
                     from={from} to={to}"
                );
            }
        }
    }

    #[test]
    fn only_ready_serves_and_names_match_the_api() {
        let ready: Vec<Lifecycle> = Lifecycle::ALL
            .into_iter()
            .filter(|state| state.is_ready())
            .collect();
        assert_eq!(
            ready,
            vec![Lifecycle::Ready],
            "rule: only Ready serves reads (A-2)"
        );
        let names: Vec<&str> = Lifecycle::ALL.iter().map(|state| state.as_str()).collect();
        assert_eq!(
            names,
            ["starting", "recovering", "ready", "failed"],
            "rule: the API's Lifecycle enum (O-1)"
        );
    }
}
