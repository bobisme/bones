//! Projection-level simulation campaign (bn-2fs6). See `bones_sim::replica`.

use bones_sim::replica::{
    Plant, Profile, Shapes, Step, Violation, campaign, drive, generate, shapes,
};

fn seed_count() -> u64 {
    std::env::var("BONES_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48)
}

/// Every seed converges: incremental equals rebuild after every step, and
/// agents with the same events agree. A failure prints its seed and a
/// shrunk plan that replays it.
#[test]
fn campaign_converges() {
    let failures = campaign(0..seed_count(), Profile::default()).expect("driver");
    for f in &failures {
        eprintln!(
            "seed {} failed: {:?}\nshrunk to {} steps: {:?}\nshrunk plan: {}",
            f.seed,
            f.violation,
            f.shrunk.steps.len(),
            f.shrunk_violation,
            f.shrunk.to_json()
        );
    }
    assert!(failures.is_empty(), "{} failing seeds", failures.len());
}

/// Determinism self-check: the generator is a pure function of its seed.
/// If this digest changes without a deliberate generator change, something
/// nondeterministic leaked into generation.
#[test]
fn plan_generation_is_deterministic() {
    let a = generate(7, Profile::default());
    let b = generate(7, Profile::default());
    assert_eq!(a, b);
    assert_eq!(
        a.digest(),
        include_str!("projection_sim_seed7.digest").trim()
    );
}

/// The campaign reaches every interleaving class and fault it exists for.
#[test]
fn hostile_shapes_are_reached() {
    let mut total = Shapes::default();
    for seed in 0..seed_count() {
        let s = shapes(&generate(seed, Profile::default()));
        total.skew_inversions += s.skew_inversions;
        total.concurrent_same_field += s.concurrent_same_field;
        total.redact_before_target += s.redact_before_target;
        total.rebase_pulls += s.rebase_pulls;
        total.faults[0] += s.faults[0];
        total.faults[1] += s.faults[1];
    }
    eprintln!("shapes over {} seeds: {total:?}", seed_count());
    assert!(total.skew_inversions > 0, "no clock-skew inversions");
    assert!(
        total.concurrent_same_field > 0,
        "no concurrent same-field writes"
    );
    assert!(
        total.redact_before_target > 0,
        "no redaction before its target"
    );
    assert!(total.rebase_pulls > 0, "no rebase pulls");
    assert!(total.faults.iter().all(|&n| n > 0), "a fault never fired");
}

/// Planted defect: corrupting a projection mid-run must trip the
/// incremental-equals-rebuild oracle at exactly that step.
#[test]
fn planted_projection_corruption_is_detected() {
    let plan = generate(1, Profile::default());
    let (index, step) = plan
        .steps
        .iter()
        .enumerate()
        .skip(10)
        .find(|(_, s)| matches!(s, Step::Create { .. } | Step::Write { .. }))
        .expect("an event step after step 10");
    let violation = drive(&plan, Some(Plant::CorruptAfterStep(index)))
        .expect("driver")
        .expect_err("planted corruption must be detected");
    match violation {
        Violation::IncrementalMismatch { step: s, agent, .. } => {
            assert_eq!(s, Some(index));
            assert_eq!(agent, step.actor());
        }
        other => panic!("wrong oracle fired: {other:?}"),
    }
}

/// Planted defect: one replica disagreeing after the final sync must trip
/// the convergence oracle.
#[test]
fn planted_divergence_is_detected() {
    let plan = generate(2, Profile::default());
    let violation = drive(&plan, Some(Plant::CorruptAtEnd(0)))
        .expect("driver")
        .expect_err("planted divergence must be detected");
    assert!(
        matches!(
            violation,
            Violation::Divergence {
                step: None,
                agents: (0, _),
                ..
            }
        ),
        "wrong oracle fired: {violation:?}"
    );
}

fn replay_fixture(json: &str) {
    let plan: bones_sim::replica::Plan = serde_json::from_str(json).expect("fixture plan");
    if let Err(v) = drive(&plan, None).expect("driver") {
        panic!("regression reappeared: {v:?}");
    }
}

/// Shrunk from seed 14. A rebase pull drops a duplicated line, so the log
/// gets shorter than the projection cursor. Lines appended later started
/// before the stale offset and were skipped: incremental apply silently
/// missed events. Fixed by the cursor prefix digest (schema v4).
#[test]
fn regression_rebase_drops_duplicate_line_skips_events() {
    replay_fixture(include_str!(
        "fixtures/rebase_drops_duplicate_line_skips_events.json"
    ));
}

/// Shrunk from seed 15. A rebase pull reorders the log, and the stale
/// cursor offset lands mid-line: incremental apply failed with a parse
/// error instead of rebuilding.
#[test]
fn regression_rebase_reorders_log_resumes_mid_line() {
    replay_fixture(include_str!(
        "fixtures/rebase_reorders_log_resumes_mid_line.json"
    ));
}
