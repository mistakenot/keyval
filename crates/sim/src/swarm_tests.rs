//! W37.2's tests of swarm configurations (PLAN.md §10.4): the non-empty fault subsets and
//! intensities seeds draw, the no-fault control, and the manifest record.
#![cfg(test)]

use std::collections::BTreeSet;

use serde_json::json;

use crate::entropy::{MasterSeed, Subsystem};
use crate::swarm::{FaultKind, FaultSet, Intensity, Swarm};

/// Seeds the draw tests use.
const SEEDS: u64 = 500;

fn draw(seed: u64) -> Swarm {
    Swarm::draw(&mut MasterSeed::new(seed).stream(Subsystem::Faults))
}

#[test]
fn a_fault_set_is_never_empty_and_remembers_its_kinds() {
    assert_eq!(
        FaultSet::new(&[]),
        None,
        "rule: no fault type enabled is not a fault set (CD-2)"
    );
    let set = FaultSet::new(&[FaultKind::Pause, FaultKind::CrashRestart, FaultKind::Pause])
        .expect("two kinds");
    assert_eq!(
        set.kinds(),
        vec![FaultKind::CrashRestart, FaultKind::Pause],
        "rule: kinds are listed once, in the fixed order"
    );
    for kind in FaultKind::ALL {
        assert_eq!(
            set.contains(kind),
            [FaultKind::CrashRestart, FaultKind::Pause].contains(&kind),
            "rule: membership; kind={kind:?}"
        );
        assert_eq!(
            FaultKind::parse(kind.name()),
            Some(kind),
            "rule: a kind's name parses back; kind={kind:?}"
        );
    }
    let names: BTreeSet<&str> = FaultKind::ALL.iter().map(|kind| kind.name()).collect();
    assert_eq!(
        names.len(),
        FaultKind::ALL.len(),
        "rule: names are distinct"
    );
}

#[test]
fn every_drawn_swarm_enables_something_and_is_decided_by_its_seed() {
    let mut kinds_seen = BTreeSet::new();
    let mut kinds_missing = BTreeSet::new();
    let mut intensities = BTreeSet::new();
    let mut sizes = BTreeSet::new();
    for seed in 0..SEEDS {
        let swarm = draw(seed);
        assert_eq!(
            swarm,
            draw(seed),
            "rule: the same seed draws the same swarm; seed={seed}"
        );
        let Swarm::Faulty { faults, intensity } = swarm else {
            panic!("rule: a drawn swarm is never the control; seed={seed} swarm={swarm:?}");
        };
        let enabled = faults.kinds();
        assert!(
            !enabled.is_empty(),
            "rule: a drawn swarm enables at least one fault type (§10.4); seed={seed}"
        );
        sizes.insert(enabled.len());
        intensities.insert(intensity.name());
        for kind in FaultKind::ALL {
            if enabled.contains(&kind) {
                kinds_seen.insert(kind);
            } else {
                kinds_missing.insert(kind);
            }
        }
    }
    assert_eq!(
        kinds_seen.len(),
        FaultKind::ALL.len(),
        "rule: every fault type is enabled by some seed; seen={kinds_seen:?}"
    );
    assert_eq!(
        kinds_missing.len(),
        FaultKind::ALL.len(),
        "rule: every fault type is left out by some seed (swarm testing varies the mix); missing={kinds_missing:?}"
    );
    assert_eq!(
        intensities.len(),
        Intensity::ALL.len(),
        "rule: every intensity is drawn; seen={intensities:?}"
    );
    assert!(
        sizes.contains(&1) && sizes.iter().any(|size| *size >= 6),
        "rule: subsets range from one fault type to most of them; sizes={sizes:?}"
    );
}

#[test]
fn harder_intensities_hit_harder() {
    let [low, medium, high] = Intensity::ALL;
    for (milder, harsher) in [(low, medium), (medium, high)] {
        assert!(
            milder.episodes() < harsher.episodes()
                && milder.chaos_ms() < harsher.chaos_ms()
                && milder.loss_ppm() < harsher.loss_ppm()
                && milder.duplicate_ppm() < harsher.duplicate_ppm()
                && milder.max_delay_us() < harsher.max_delay_us(),
            "rule: {milder:?} is milder than {harsher:?} in every dimension"
        );
    }
    for intensity in Intensity::ALL {
        assert_eq!(
            Intensity::parse(intensity.name()),
            Some(intensity),
            "rule: an intensity's name parses back; intensity={intensity:?}"
        );
        assert!(
            intensity.loss_ppm() <= 1_000_000 && intensity.duplicate_ppm() <= 1_000_000,
            "rule: probabilities are at most one; intensity={intensity:?}"
        );
    }
}

#[test]
fn the_manifest_records_a_swarm_and_reads_back() {
    assert_eq!(
        Swarm::Control.manifest(),
        json!({"control": true, "faults": [], "intensity": null}),
        "rule: the control's manifest enables nothing"
    );
    let faulty = Swarm::Faulty {
        faults: FaultSet::new(&[FaultKind::MessageLoss, FaultKind::Pause]).unwrap(),
        intensity: Intensity::High,
    };
    assert_eq!(
        faulty.manifest(),
        json!({"control": false, "faults": ["message_loss", "pause"], "intensity": "high"}),
        "rule: a faulty swarm's manifest lists its fault types and its intensity"
    );
    for seed in 0..SEEDS {
        for swarm in [draw(seed), Swarm::Control] {
            assert_eq!(
                Swarm::from_json(&swarm.manifest()),
                Ok(swarm),
                "rule: a manifest reads back; seed={seed}"
            );
        }
    }
    assert!(
        faulty.enables(FaultKind::Pause) && !faulty.enables(FaultKind::Duplication),
        "rule: a swarm enables what its set holds"
    );
    assert!(
        FaultKind::ALL
            .iter()
            .all(|kind| !Swarm::Control.enables(*kind)),
        "rule: the control enables nothing"
    );
}

#[test]
fn a_manifest_that_could_not_have_been_written_is_refused() {
    let bad = [
        (
            json!({"control": false, "faults": [], "intensity": "low"}),
            "a faulty swarm with no fault types",
        ),
        (
            json!({"control": false, "faults": ["pause"], "intensity": "extreme"}),
            "an unknown intensity",
        ),
        (
            json!({"control": false, "faults": ["meteor"], "intensity": "low"}),
            "an unknown fault type",
        ),
        (
            json!({"control": false, "faults": ["pause"], "intensity": null}),
            "a faulty swarm with no intensity",
        ),
        (
            json!({"control": true, "faults": ["pause"], "intensity": null}),
            "a control that enables a fault",
        ),
        (
            json!({"control": true, "faults": [], "intensity": "low"}),
            "a control with an intensity",
        ),
        (json!({"faults": ["pause"]}), "a missing control flag"),
        (
            json!({"control": false, "intensity": "low"}),
            "missing fault types",
        ),
        (
            json!({"control": false, "faults": [7], "intensity": "low"}),
            "a fault type that is not a string",
        ),
    ];
    for (value, what) in bad {
        let verdict = Swarm::from_json(&value);
        assert!(
            verdict.is_err(),
            "rule: {what} is refused; manifest={value} verdict={verdict:?}"
        );
    }
}
