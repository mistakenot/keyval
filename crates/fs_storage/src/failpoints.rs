//! Test-only failpoints (PLAN.md §10.17, T-2, T-6; W24.1).
//!
//! This module exists only under the `failpoints` feature, which no production crate
//! enables: the crash probes (`tests/crash_probes.rs`) turn it on through this crate's own
//! dev-dependency, and `tests/failpoints_absent.rs` proves a release build has neither the
//! feature nor this module's code. A failpoint sits on the OS-call seam, so it changes no
//! decision the adapter makes: when armed and reached, the process announces the point on
//! standard output and stops there, holding every open file and lock exactly as the call
//! left them. The parent then kills it with `SIGKILL` and examines what the files hold.

use std::io::Write as _;
use std::sync::Mutex;

/// The announcement a process prints, followed by the point's [`Point::name`], when it
/// stops at its armed failpoint. The parent reads this line, so it is the whole protocol.
pub const MARKER: &str = "keyval-failpoint-reached";

/// A place in the adapter's durability calls where a probe can stop the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    /// Before an append writes any byte.
    BeforeAppend,
    /// In the middle of an append: the first half of its bytes are written, the rest not.
    MidAppend,
    /// After an append wrote all its bytes, before the sync that would make them durable.
    AfterAppend,
    /// After `fdatasync` returned, before the caller observes the completion.
    AfterSync,
    /// After `mkdir` created the data directory, before its parent is synced.
    AfterMkdir,
    /// After the journal file was created, before its directory entry is synced.
    AfterJournalCreate,
    /// During tail truncation, before the journal is shortened.
    BeforeTruncate,
    /// During tail truncation, after the journal was shortened, before the sync.
    AfterTruncate,
}

impl Point {
    /// Every point, in declaration order.
    pub const ALL: [Self; 8] = [
        Self::BeforeAppend,
        Self::MidAppend,
        Self::AfterAppend,
        Self::AfterSync,
        Self::AfterMkdir,
        Self::AfterJournalCreate,
        Self::BeforeTruncate,
        Self::AfterTruncate,
    ];

    /// The point's name on the command line and in the announcement.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::BeforeAppend => "before-append",
            Self::MidAppend => "mid-append",
            Self::AfterAppend => "after-append",
            Self::AfterSync => "after-sync",
            Self::AfterMkdir => "after-mkdir",
            Self::AfterJournalCreate => "after-journal-create",
            Self::BeforeTruncate => "before-truncate",
            Self::AfterTruncate => "after-truncate",
        }
    }

    /// The point with this name, if any.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|point| point.name() == name)
    }
}

/// The armed point and how many arrivals at it to let pass first. Bounded: one slot.
static ARMED: Mutex<Option<(Point, u32)>> = Mutex::new(None);

/// Arms `point` for this process: it stops at the arrival after `skip` earlier ones.
pub fn arm(point: Point, skip: u32) {
    if let Ok(mut slot) = ARMED.lock() {
        *slot = Some((point, skip));
    }
}

/// An arrival at `point`. Does nothing unless it is armed and its skips are used up; then
/// announces itself and never returns (the parent kills the process).
pub(crate) fn hit(point: Point) {
    let reached = ARMED.lock().is_ok_and(|mut slot| match *slot {
        Some((armed, 0)) if armed == point => true,
        Some((armed, skip)) if armed == point => {
            *slot = Some((armed, skip - 1));
            false
        }
        Some(_) | None => false,
    });
    if reached {
        let mut out = std::io::stdout().lock();
        // A failed announcement leaves the parent without its line; it fails on the EOF.
        let _announced = writeln!(out, "{MARKER} {}", point.name()).and_then(|()| out.flush());
        loop {
            std::thread::park();
        }
    }
}
