//! Projection-level deterministic simulation (bn-2fs6).
//!
//! The grow-only-set simulator in the crate root checks the network model
//! only. This module runs the real system: each simulated agent owns a real
//! `.bones` directory, writes real events with a skewed wall clock, pulls
//! other agents' logs the way git does (union merge or rebase), and projects
//! them through `bones_core`'s incremental apply and full rebuild.
//!
//! ```text
//! (seed, profile) --> generate --> Plan --> drive --> Outcome / Violation
//!                     (pure)      (data)   (effects, oracles after every step)
//! ```
//!
//! # Determinism contract
//!
//! [`generate`] returns a byte-identical [`Plan`] for a given
//! `(seed, profile)` on every run and machine: it uses only
//! [`DeterministicRng`], ordered collections and integer arithmetic, and it
//! never touches the file system or `bones_core` state. Every timestamp in a
//! plan comes from a seeded [`SimulatedClock`]. The driver writes events into
//! a fixed shard, so the real clock never reaches the event log.
//!
//! # Oracles
//!
//! After every step, for the agent that acted:
//! - **Incremental equals rebuild**: its projection after the incremental
//!   apply equals a full rebuild of its log into a scratch database.
//! - **Convergence**: any two agents that hold the same set of events have
//!   identical projections.
//!
//! At the end every agent pulls from every other, and all projections must
//! be identical.
//!
//! # Faults
//!
//! A closed vocabulary, see [`Fault`]: a duplicated log line (what a union
//! merge produces when two replicas pulled the same event), a deleted
//! projection database, and rebase pulls that rewrite the log.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bones_core::db::incremental::incremental_apply;
use bones_core::db::rebuild::rebuild;
use bones_core::event::Event;
use bones_core::event::data::{
    AssignAction, AssignData, CommentData, CreateData, DeleteData, EventData, LinkData, MoveData,
    RedactData, UnlinkData, UpdateData,
};
use bones_core::event::types::EventType;
use bones_core::event::writer::{shard_header, write_event};
use bones_core::model::item::{Kind, State, Urgency};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::clock::{ClockSpec, SimulatedClock};
use crate::rng::DeterministicRng;

// ---------------------------------------------------------------------------
// Profile and plan
// ---------------------------------------------------------------------------

/// Shape of generated plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Number of agents (replicas).
    pub agents: usize,
    /// Number of distinct item IDs that may be created.
    pub items: usize,
    /// Number of steps before the final all-to-all sync.
    pub steps: usize,
    /// Wall-clock milliseconds per step.
    pub tick_millis: i64,
    /// Maximum absolute per-agent clock skew in milliseconds. Larger than a
    /// few ticks, so a causally later write can carry an earlier timestamp.
    pub max_abs_skew_millis: i64,
    /// Maximum absolute per-agent clock drift in parts per million.
    pub max_abs_drift_ppm: i32,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            agents: 3,
            items: 3,
            steps: 40,
            tick_millis: 100,
            max_abs_skew_millis: 1_000,
            max_abs_drift_ppm: 50_000,
        }
    }
}

/// A field-level write to an existing item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteOp {
    /// Set the title.
    Title(u8),
    /// Set the description.
    Description(u8),
    /// Move to a state: 0 open, 1 doing, 2 done.
    Move(u8),
    /// Add or remove one label.
    Label {
        /// Add when true, remove when false.
        add: bool,
        /// Label index.
        label: u8,
    },
    /// Legacy whole-set label replacement: the labels whose bits are set.
    LabelsReplace(u8),
    /// Assign or unassign a person.
    Assign {
        /// Assign when true, unassign when false.
        assign: bool,
        /// Person index.
        person: u8,
    },
    /// Link to, or remove a link to, another item. A link always has a
    /// type. A removal without one removes every link type to the target.
    Link {
        /// Target item index.
        target: usize,
        /// Link when true, remove when false.
        link: bool,
        /// Link type index, if any.
        link_type: Option<u8>,
    },
    /// Add a comment.
    Comment(u8),
    /// Soft-delete the item.
    Delete,
}

/// A named fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Fault {
    /// `FP_DUPLICATE_LINE`: re-append a line the agent already has, as a
    /// union merge does when both sides pulled the same event.
    DuplicateLine {
        /// Line index, taken modulo the log length.
        pick: u16,
    },
    /// `FP_DROP_PROJECTION`: delete the projection database, which forces
    /// the next apply to rebuild from the log.
    DropProjection,
}

/// How a pull merges the other agent's log into this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PullMode {
    /// `git pull` with `merge=union`: our lines first, then their new lines.
    Union,
    /// `git pull --rebase`: their log first, then our lines they lack.
    Rebase,
}

/// One plan step. Event steps carry the acting agent's clock reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// Create item `item`.
    Create {
        /// Acting agent.
        agent: usize,
        /// Item index.
        item: usize,
        /// Event wall clock, microseconds.
        wall_ts_us: i64,
    },
    /// Write a field of item `item`.
    Write {
        /// Acting agent.
        agent: usize,
        /// Item index.
        item: usize,
        /// The write.
        op: WriteOp,
        /// Event wall clock, microseconds.
        wall_ts_us: i64,
    },
    /// Redact the event emitted by plan step `target_step`.
    Redact {
        /// Acting agent.
        agent: usize,
        /// Item the redaction event is filed under.
        item: usize,
        /// Plan index of the redacted event.
        target_step: usize,
        /// Event wall clock, microseconds.
        wall_ts_us: i64,
    },
    /// Agent `to` pulls agent `from`'s log.
    Pull {
        /// Receiving agent.
        to: usize,
        /// Sending agent.
        from: usize,
        /// Merge strategy.
        mode: PullMode,
    },
    /// Inject a fault at `agent`.
    Fault {
        /// Affected agent.
        agent: usize,
        /// The fault.
        fault: Fault,
    },
}

impl Step {
    /// Agent whose log or projection this step changes.
    #[must_use]
    pub const fn actor(&self) -> usize {
        match *self {
            Self::Create { agent, .. }
            | Self::Write { agent, .. }
            | Self::Redact { agent, .. }
            | Self::Fault { agent, .. } => agent,
            Self::Pull { to, .. } => to,
        }
    }
}

/// A generated plan: inert, serializable data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Generating seed.
    pub seed: u64,
    /// Generating profile.
    pub profile: Profile,
    /// Per-agent clocks.
    pub clocks: Vec<ClockSpec>,
    /// Steps, in order.
    pub steps: Vec<Step>,
}

impl Plan {
    /// Canonical JSON (struct field order, no maps).
    ///
    /// # Panics
    ///
    /// Never: every plan type serializes.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("plan serializes")
    }

    /// BLAKE3 digest of the canonical JSON, for the determinism self-check.
    #[must_use]
    pub fn digest(&self) -> String {
        blake3::hash(self.to_json().as_bytes()).to_hex().to_string()
    }
}

// ---------------------------------------------------------------------------
// Generator (pure)
// ---------------------------------------------------------------------------

fn signed_in(rng: &mut DeterministicRng, max_abs: i64) -> i64 {
    let span = u64::try_from(max_abs).unwrap_or(0).saturating_mul(2) + 1;
    i64::try_from(rng.next_bounded(span)).unwrap_or(0) - max_abs
}

fn pick<T: Copy>(rng: &mut DeterministicRng, from: &[T]) -> Option<T> {
    if from.is_empty() {
        return None;
    }
    let len = u64::try_from(from.len()).unwrap_or(u64::MAX);
    let i = usize::try_from(rng.next_bounded(len)).unwrap_or(0);
    from.get(i).copied()
}

fn small(rng: &mut DeterministicRng, upper: u8) -> u8 {
    u8::try_from(rng.next_bounded(u64::from(upper))).unwrap_or(0)
}

fn gen_op(rng: &mut DeterministicRng, item: usize, known_items: &[usize]) -> WriteOp {
    match rng.next_bounded(9) {
        0 => WriteOp::Title(small(rng, 4)),
        1 => WriteOp::Description(small(rng, 4)),
        2 => WriteOp::Move(small(rng, 3)),
        3 => WriteOp::Label {
            add: rng.hit_rate_percent(60),
            label: small(rng, 3),
        },
        4 => WriteOp::LabelsReplace(small(rng, 8)),
        5 => WriteOp::Assign {
            assign: rng.hit_rate_percent(60),
            person: small(rng, 2),
        },
        6 => {
            let others: Vec<usize> = known_items.iter().copied().filter(|&i| i != item).collect();
            match pick(rng, &others) {
                Some(target) => {
                    let link = rng.hit_rate_percent(60);
                    let typed = link || rng.hit_rate_percent(50);
                    WriteOp::Link {
                        target,
                        link,
                        link_type: typed.then(|| small(rng, 2)),
                    }
                }
                None => WriteOp::Title(small(rng, 4)),
            }
        }
        7 => WriteOp::Comment(small(rng, 4)),
        _ => {
            if rng.hit_rate_percent(20) {
                WriteOp::Delete
            } else {
                WriteOp::Title(small(rng, 4))
            }
        }
    }
}

/// Abstract model the generator keeps: which event steps each agent knows,
/// and which items each event step created.
struct Model {
    known: Vec<BTreeSet<usize>>,
    creates: BTreeMap<usize, usize>,
    created: BTreeSet<usize>,
}

impl Model {
    fn known_items(&self, agent: usize) -> Vec<usize> {
        self.known[agent]
            .iter()
            .filter_map(|s| self.creates.get(s).copied())
            .collect()
    }
}

/// Generate a plan. Pure: same `(seed, profile)`, same plan.
///
/// # Panics
///
/// Panics if `profile.agents` is below 2 or `profile.items` is 0.
#[must_use]
pub fn generate(seed: u64, profile: Profile) -> Plan {
    assert!(profile.agents >= 2, "need at least two agents");
    assert!(profile.items >= 1, "need at least one item");
    let mut rng = DeterministicRng::new(seed);

    let clocks: Vec<ClockSpec> = (0..profile.agents)
        .map(|_| ClockSpec {
            base_millis: 1_700_000_000_000,
            tick_millis: profile.tick_millis,
            drift_ppm: i32::try_from(signed_in(&mut rng, i64::from(profile.max_abs_drift_ppm)))
                .unwrap_or(0),
            skew_millis: signed_in(&mut rng, profile.max_abs_skew_millis),
        })
        .collect();

    let mut model = Model {
        known: vec![BTreeSet::new(); profile.agents],
        creates: BTreeMap::new(),
        created: BTreeSet::new(),
    };
    let agents: Vec<usize> = (0..profile.agents).collect();
    let mut steps = Vec::with_capacity(profile.steps);

    for round in 0..profile.steps {
        let index = steps.len();
        let agent = pick(&mut rng, &agents).unwrap_or(0);
        let round_u64 = u64::try_from(round).unwrap_or(u64::MAX);
        // Sub-millisecond jitter keeps equal readings rare but possible.
        let wall_ts_us = SimulatedClock::new(clocks[agent])
            .now_millis(round_u64)
            .saturating_mul(1_000)
            + i64::try_from(rng.next_bounded(3)).unwrap_or(0);
        let known_items = model.known_items(agent);
        let uncreated: Vec<usize> = (0..profile.items)
            .filter(|i| !model.created.contains(i))
            .collect();

        let roll = rng.next_bounded(100);
        let step = if known_items.is_empty() || (roll < 6 && !uncreated.is_empty()) {
            match pick(&mut rng, &uncreated) {
                Some(item) => Step::Create {
                    agent,
                    item,
                    wall_ts_us,
                },
                None => pull_step(&mut rng, agent, &agents),
            }
        } else if roll < 60 {
            let item = pick(&mut rng, &known_items).unwrap_or(0);
            Step::Write {
                agent,
                item,
                op: gen_op(&mut rng, item, &known_items),
                wall_ts_us,
            }
        } else if roll < 68 {
            let targets: Vec<usize> = model.known[agent].iter().copied().collect();
            let target_step = pick(&mut rng, &targets).unwrap_or(0);
            let item = pick(&mut rng, &known_items).unwrap_or(0);
            Step::Redact {
                agent,
                item,
                target_step,
                wall_ts_us,
            }
        } else if roll < 93 {
            pull_step(&mut rng, agent, &agents)
        } else if rng.hit_rate_percent(50) {
            Step::Fault {
                agent,
                fault: Fault::DuplicateLine {
                    pick: u16::try_from(rng.next_bounded(1 << 16)).unwrap_or(0),
                },
            }
        } else {
            Step::Fault {
                agent,
                fault: Fault::DropProjection,
            }
        };

        match &step {
            Step::Create { agent, item, .. } => {
                model.known[*agent].insert(index);
                model.creates.insert(index, *item);
                model.created.insert(*item);
            }
            Step::Write { agent, .. } | Step::Redact { agent, .. } => {
                model.known[*agent].insert(index);
            }
            Step::Pull { to, from, .. } => {
                let theirs = model.known[*from].clone();
                model.known[*to].extend(theirs);
            }
            Step::Fault { .. } => {}
        }
        steps.push(step);
    }

    Plan {
        seed,
        profile,
        clocks,
        steps,
    }
}

fn pull_step(rng: &mut DeterministicRng, to: usize, agents: &[usize]) -> Step {
    let others: Vec<usize> = agents.iter().copied().filter(|&a| a != to).collect();
    Step::Pull {
        to,
        from: pick(rng, &others).unwrap_or(0),
        mode: if rng.hit_rate_percent(30) {
            PullMode::Rebase
        } else {
            PullMode::Union
        },
    }
}

// ---------------------------------------------------------------------------
// Hostile shapes (pure analysis of a plan)
// ---------------------------------------------------------------------------

/// Counts of the interleavings this simulation exists to reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shapes {
    /// A write that causally follows a write to the same field of the same
    /// item but carries an earlier wall clock (clock skew inversion).
    pub skew_inversions: usize,
    /// Two writes to the same field of the same item where the later one's
    /// author had not seen the earlier one.
    pub concurrent_same_field: usize,
    /// A redaction whose wall clock is earlier than its target's.
    pub redact_before_target: usize,
    /// Pulls in rebase mode.
    pub rebase_pulls: usize,
    /// Faults injected, by kind: duplicate line, dropped projection.
    pub faults: [usize; 2],
}

fn field_key(item: usize, op: &WriteOp) -> Option<(usize, String)> {
    let field = match op {
        WriteOp::Title(_) => "title".to_string(),
        WriteOp::Description(_) => "description".to_string(),
        WriteOp::Move(_) => "state".to_string(),
        WriteOp::Label { label, .. } => format!("label/{label}"),
        WriteOp::Assign { person, .. } => format!("assignee/{person}"),
        WriteOp::Delete => "deleted".to_string(),
        WriteOp::LabelsReplace(_) | WriteOp::Link { .. } | WriteOp::Comment(_) => return None,
    };
    Some((item, field))
}

/// Count the hostile interleavings a plan contains.
#[must_use]
pub fn shapes(plan: &Plan) -> Shapes {
    let mut out = Shapes::default();
    let mut known: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); plan.profile.agents];
    // (item, field) -> earlier writes as (step, wall_ts).
    let mut writes: BTreeMap<(usize, String), Vec<(usize, i64)>> = BTreeMap::new();
    let mut ts_of: BTreeMap<usize, i64> = BTreeMap::new();

    for (index, step) in plan.steps.iter().enumerate() {
        match step {
            Step::Create {
                agent, wall_ts_us, ..
            } => {
                known[*agent].insert(index);
                ts_of.insert(index, *wall_ts_us);
            }
            Step::Write {
                agent,
                item,
                op,
                wall_ts_us,
            } => {
                if let Some(key) = field_key(*item, op) {
                    for &(earlier, earlier_ts) in writes.get(&key).map_or(&[][..], Vec::as_slice) {
                        if known[*agent].contains(&earlier) {
                            if *wall_ts_us < earlier_ts {
                                out.skew_inversions += 1;
                            }
                        } else {
                            out.concurrent_same_field += 1;
                        }
                    }
                    writes.entry(key).or_default().push((index, *wall_ts_us));
                }
                known[*agent].insert(index);
                ts_of.insert(index, *wall_ts_us);
            }
            Step::Redact {
                agent,
                target_step,
                wall_ts_us,
                ..
            } => {
                if ts_of.get(target_step).is_some_and(|t| wall_ts_us < t) {
                    out.redact_before_target += 1;
                }
                known[*agent].insert(index);
                ts_of.insert(index, *wall_ts_us);
            }
            Step::Pull { to, from, mode } => {
                let theirs = known[*from].clone();
                known[*to].extend(theirs);
                if *mode == PullMode::Rebase {
                    out.rebase_pulls += 1;
                }
            }
            Step::Fault { fault, .. } => match fault {
                Fault::DuplicateLine { .. } => out.faults[0] += 1,
                Fault::DropProjection => out.faults[1] += 1,
            },
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// A deliberate defect the driver can inject, to prove the oracles notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plant {
    /// Corrupt the acting agent's projection right after step `n` applies.
    CorruptAfterStep(usize),
    /// Corrupt agent `n`'s projection after the final sync.
    CorruptAtEnd(usize),
}

/// What an oracle found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Violation {
    /// An agent's incrementally applied projection differs from a full
    /// rebuild of its own log.
    IncrementalMismatch {
        /// Plan step, or `None` for the final sync.
        step: Option<usize>,
        /// Agent.
        agent: usize,
        /// First differing rows: (incremental, rebuild).
        diff: (Option<String>, Option<String>),
    },
    /// The projection could not be brought up to date from the agent's log.
    ApplyFailed {
        /// Plan step, or `None` for the final sync.
        step: Option<usize>,
        /// Agent.
        agent: usize,
        /// Error chain.
        error: String,
    },
    /// Two agents with the same events have different projections.
    Divergence {
        /// Plan step, or `None` for the final sync.
        step: Option<usize>,
        /// Agents compared.
        agents: (usize, usize),
        /// First differing rows.
        diff: (Option<String>, Option<String>),
    },
}

impl Violation {
    /// Short kind name, used to keep a shrunk plan failing the same way.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::IncrementalMismatch { .. } => "incremental-mismatch",
            Self::ApplyFailed { .. } => "apply-failed",
            Self::Divergence { .. } => "divergence",
        }
    }
}

/// Result of a clean run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Events in each agent's log at the end.
    pub events_per_agent: Vec<usize>,
    /// Distinct events across all agents.
    pub distinct_events: usize,
}

struct Replica {
    _dir: TempDir,
    bones_dir: PathBuf,
    shard: (i32, u32),
    lines: Vec<String>,
    hashes: BTreeSet<String>,
    snapshot: Vec<String>,
}

impl Replica {
    fn new() -> Result<Self> {
        let dir = TempDir::new().context("create replica dir")?;
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).context("create .bones")?;
        let shard = ShardManager::new(&bones_dir)
            .init()
            .context("init shards")?;
        Ok(Self {
            _dir: dir,
            bones_dir,
            shard,
            lines: Vec::new(),
            hashes: BTreeSet::new(),
            snapshot: Vec::new(),
        })
    }

    fn shard_path(&self) -> PathBuf {
        ShardManager::new(&self.bones_dir).shard_path(self.shard.0, self.shard.1)
    }

    fn append(&mut self, line: &str) -> Result<()> {
        ShardManager::new(&self.bones_dir)
            .append_raw(self.shard.0, self.shard.1, line)
            .context("append line")?;
        self.lines.push(line.to_string());
        if let Some(hash) = line_hash(line) {
            self.hashes.insert(hash);
        }
        Ok(())
    }

    fn rewrite(&mut self, lines: Vec<String>) -> Result<()> {
        let mut content = shard_header();
        for line in &lines {
            content.push_str(line);
        }
        std::fs::write(self.shard_path(), content).context("rewrite shard")?;
        self.hashes = lines.iter().filter_map(|l| line_hash(l)).collect();
        self.lines = lines;
        Ok(())
    }

    fn db_path(&self) -> PathBuf {
        self.bones_dir.join("bones.db")
    }

    fn drop_projection(&self) -> Result<()> {
        for ext in ["db", "db-wal", "db-shm"] {
            let path = self.bones_dir.join(format!("bones.{ext}"));
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("remove {}", path.display()))?;
            }
        }
        Ok(())
    }

    /// Incremental apply, then check it against a full rebuild of the log.
    fn apply_and_check(
        &mut self,
        agent: usize,
        step: Option<usize>,
        corrupt: bool,
    ) -> Result<Option<Violation>> {
        let events_dir = self.bones_dir.join("events");
        if let Err(err) = incremental_apply(&events_dir, &self.db_path(), false) {
            return Ok(Some(Violation::ApplyFailed {
                step,
                agent,
                error: format!("{err:#}"),
            }));
        }
        if corrupt {
            let conn = rusqlite::Connection::open(self.db_path())?;
            conn.execute("UPDATE items SET title = title || ' (planted)'", [])?;
        }
        self.snapshot = snapshot(&self.db_path())?;

        let check_db = self.bones_dir.join("check.db");
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(self.bones_dir.join(format!("check.{ext}")));
        }
        rebuild(&events_dir, &check_db).context("check rebuild")?;
        let rebuilt = snapshot(&check_db)?;
        if rebuilt != self.snapshot {
            return Ok(Some(Violation::IncrementalMismatch {
                step,
                agent,
                diff: first_diff(&self.snapshot, &rebuilt),
            }));
        }
        Ok(None)
    }
}

/// Event hash of a log line (last tab-separated field), if it is an event.
fn line_hash(line: &str) -> Option<String> {
    if line.starts_with('#') {
        return None;
    }
    line.trim_end()
        .rsplit('\t')
        .next()
        .filter(|h| h.starts_with("blake3:"))
        .map(str::to_string)
}

fn first_diff(a: &[String], b: &[String]) -> (Option<String>, Option<String>) {
    let n = a.len().max(b.len());
    for i in 0..n {
        if a.get(i) != b.get(i) {
            return (a.get(i).cloned(), b.get(i).cloned());
        }
    }
    (None, None)
}

/// Everything a user can observe in the projection, in a stable order.
fn snapshot(db_path: &Path) -> Result<Vec<String>> {
    let conn = rusqlite::Connection::open(db_path).context("open projection")?;
    let mut out = Vec::new();
    for (name, sql) in [
        (
            "items",
            "SELECT item_id, title, description, kind, state, urgency, size, parent_id, \
             is_deleted, deleted_at_us, search_labels, created_at_us, updated_at_us \
             FROM items ORDER BY item_id",
        ),
        (
            "labels",
            "SELECT item_id, label, created_at_us FROM item_labels ORDER BY item_id, label",
        ),
        (
            "assignees",
            "SELECT item_id, agent, created_at_us FROM item_assignees ORDER BY item_id, agent",
        ),
        (
            "deps",
            "SELECT item_id, depends_on_item_id, link_type, created_at_us \
             FROM item_dependencies ORDER BY item_id, depends_on_item_id, link_type",
        ),
        (
            "comments",
            "SELECT item_id, event_hash, author, body, created_at_us FROM item_comments \
             ORDER BY event_hash",
        ),
        (
            "redactions",
            "SELECT target_event_hash, item_id, reason, redacted_by, redacted_at_us \
             FROM event_redactions ORDER BY target_event_hash",
        ),
    ] {
        let mut stmt = conn.prepare(sql)?;
        let columns = stmt.column_count();
        let rows = stmt.query_map([], |row| {
            let mut parts = Vec::with_capacity(columns);
            for i in 0..columns {
                let value: rusqlite::types::Value = row.get(i)?;
                parts.push(format!("{value:?}"));
            }
            Ok(format!("{name}: {}", parts.join(" | ")))
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

fn item_id(item: usize) -> String {
    format!("bn-sim{item}")
}

fn base_event(agent: usize, item: usize, wall_ts_us: i64, t: EventType, d: EventData) -> Event {
    Event {
        wall_ts_us,
        agent: format!("agent-{agent}"),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: t,
        item_id: ItemId::new_unchecked(item_id(item)),
        data: d,
        event_hash: String::new(),
    }
}

fn update(field: &str, value: serde_json::Value) -> (EventType, EventData) {
    (
        EventType::Update,
        EventData::Update(UpdateData {
            field: field.to_string(),
            value,
            extra: BTreeMap::new(),
        }),
    )
}

fn link_type_name(n: u8) -> String {
    if n == 0 { "blocks" } else { "related_to" }.to_string()
}

fn write_event_data(agent: usize, op: &WriteOp) -> (EventType, EventData) {
    match op {
        WriteOp::Title(n) => update("title", serde_json::json!(format!("title {n}"))),
        WriteOp::Description(n) => update("description", serde_json::json!(format!("desc {n}"))),
        WriteOp::Move(n) => (
            EventType::Move,
            EventData::Move(MoveData {
                state: match n {
                    0 => State::Open,
                    1 => State::Doing,
                    _ => State::Done,
                },
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
        WriteOp::Label { add, label } => update(
            "labels",
            serde_json::json!({
                "action": if *add { "add" } else { "remove" },
                "label": format!("label-{label}"),
            }),
        ),
        WriteOp::LabelsReplace(bits) => update(
            "labels",
            serde_json::json!(
                (0u8..3)
                    .filter(|i| bits & (1 << i) != 0)
                    .map(|i| format!("label-{i}"))
                    .collect::<Vec<_>>()
            ),
        ),
        WriteOp::Assign { assign, person } => (
            EventType::Assign,
            EventData::Assign(AssignData {
                agent: format!("person-{person}"),
                action: if *assign {
                    AssignAction::Assign
                } else {
                    AssignAction::Unassign
                },
                extra: BTreeMap::new(),
            }),
        ),
        WriteOp::Link {
            target,
            link: true,
            link_type,
        } => (
            EventType::Link,
            EventData::Link(LinkData {
                target: item_id(*target),
                link_type: link_type_name(link_type.unwrap_or(0)),
                extra: BTreeMap::new(),
            }),
        ),
        WriteOp::Link {
            target,
            link: false,
            link_type,
        } => (
            EventType::Unlink,
            EventData::Unlink(UnlinkData {
                target: item_id(*target),
                link_type: link_type.map(link_type_name),
                extra: BTreeMap::new(),
            }),
        ),
        WriteOp::Comment(n) => (
            EventType::Comment,
            EventData::Comment(CommentData {
                body: format!("comment {n} by agent-{agent}"),
                extra: BTreeMap::new(),
            }),
        ),
        WriteOp::Delete => (
            EventType::Delete,
            EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
    }
}

/// Run a plan against real replicas, checking the oracles after every step.
///
/// Returns `Ok(Err(violation))` when an oracle fires, and `Err` only for a
/// driver failure (I/O, `SQLite`). Steps that name a missing redaction
/// target are skipped, so shrunk plans stay runnable.
///
/// # Errors
///
/// Returns an error if the driver itself fails.
pub fn drive(plan: &Plan, plant: Option<Plant>) -> Result<std::result::Result<Outcome, Violation>> {
    let agents = plan.profile.agents;
    let mut replicas: Vec<Replica> = (0..agents).map(|_| Replica::new()).collect::<Result<_>>()?;
    let mut step_hash: BTreeMap<usize, String> = BTreeMap::new();

    for (index, step) in plan.steps.iter().enumerate() {
        let actor = step.actor();
        let event = match step {
            Step::Create {
                agent,
                item,
                wall_ts_us,
            } => Some(base_event(
                *agent,
                *item,
                *wall_ts_us,
                EventType::Create,
                EventData::Create(CreateData {
                    title: format!("item {item}"),
                    kind: Kind::Task,
                    size: None,
                    urgency: Urgency::Default,
                    labels: vec!["label-0".to_string()],
                    parent: None,
                    causation: None,
                    description: Some("created".to_string()),
                    extra: BTreeMap::new(),
                }),
            )),
            Step::Write {
                agent,
                item,
                op,
                wall_ts_us,
            } => {
                let (t, d) = write_event_data(*agent, op);
                Some(base_event(*agent, *item, *wall_ts_us, t, d))
            }
            Step::Redact {
                agent,
                item,
                target_step,
                wall_ts_us,
            } => {
                let Some(target_hash) = step_hash.get(target_step) else {
                    continue;
                };
                Some(base_event(
                    *agent,
                    *item,
                    *wall_ts_us,
                    EventType::Redact,
                    EventData::Redact(RedactData {
                        target_hash: target_hash.clone(),
                        reason: "secret".to_string(),
                        extra: BTreeMap::new(),
                    }),
                ))
            }
            Step::Pull { to, from, mode } => {
                if to == from {
                    continue;
                }
                let theirs = replicas[*from].lines.clone();
                let their_hashes = replicas[*from].hashes.clone();
                let receiver = &mut replicas[*to];
                match mode {
                    PullMode::Union => {
                        for line in theirs {
                            if line_hash(&line).is_some_and(|h| !receiver.hashes.contains(&h)) {
                                receiver.append(&line)?;
                            }
                        }
                    }
                    PullMode::Rebase => {
                        let mut merged = theirs;
                        for line in &receiver.lines {
                            if line_hash(line).is_some_and(|h| !their_hashes.contains(&h)) {
                                merged.push(line.clone());
                            }
                        }
                        receiver.rewrite(merged)?;
                    }
                }
                None
            }
            Step::Fault { agent, fault } => {
                let replica = &mut replicas[*agent];
                match fault {
                    Fault::DuplicateLine { pick } => {
                        if replica.lines.is_empty() {
                            continue;
                        }
                        let line = replica.lines[usize::from(*pick) % replica.lines.len()].clone();
                        replica.append(&line)?;
                    }
                    Fault::DropProjection => replica.drop_projection()?,
                }
                None
            }
        };

        if let Some(mut event) = event {
            let line = write_event(&mut event).context("serialize event")?;
            step_hash.insert(index, event.event_hash.clone());
            replicas[actor].append(&line)?;
        }

        let corrupt = plant == Some(Plant::CorruptAfterStep(index));
        if let Some(v) = replicas[actor].apply_and_check(actor, Some(index), corrupt)? {
            return Ok(Err(v));
        }
        if let Some(v) = check_convergence(&replicas, Some(index)) {
            return Ok(Err(v));
        }
    }

    // Final sync: two rounds of all-to-all union pulls reach every event.
    for _ in 0..2 {
        for to in 0..agents {
            for from in 0..agents {
                if to == from {
                    continue;
                }
                let theirs = replicas[from].lines.clone();
                let receiver = &mut replicas[to];
                for line in theirs {
                    if line_hash(&line).is_some_and(|h| !receiver.hashes.contains(&h)) {
                        receiver.append(&line)?;
                    }
                }
            }
        }
    }
    for (agent, replica) in replicas.iter_mut().enumerate() {
        if let Some(v) = replica.apply_and_check(agent, None, false)? {
            return Ok(Err(v));
        }
    }
    if let Some(Plant::CorruptAtEnd(agent)) = plant {
        let conn = rusqlite::Connection::open(replicas[agent].db_path())?;
        conn.execute("UPDATE items SET title = title || ' (planted)'", [])?;
        replicas[agent].snapshot = snapshot(&replicas[agent].db_path())?;
    }
    if let Some(v) = check_convergence(&replicas, None) {
        return Ok(Err(v));
    }

    let distinct: BTreeSet<&String> = replicas.iter().flat_map(|r| &r.hashes).collect();
    Ok(Ok(Outcome {
        events_per_agent: replicas.iter().map(|r| r.hashes.len()).collect(),
        distinct_events: distinct.len(),
    }))
}

fn check_convergence(replicas: &[Replica], step: Option<usize>) -> Option<Violation> {
    for a in 0..replicas.len() {
        for b in (a + 1)..replicas.len() {
            let (ra, rb) = (&replicas[a], &replicas[b]);
            // Only agents that have applied their log hold a snapshot.
            if ra.hashes == rb.hashes
                && !ra.hashes.is_empty()
                && !ra.snapshot.is_empty()
                && !rb.snapshot.is_empty()
                && ra.snapshot != rb.snapshot
            {
                return Some(Violation::Divergence {
                    step,
                    agents: (a, b),
                    diff: first_diff(&ra.snapshot, &rb.snapshot),
                });
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Shrinking and campaigns
// ---------------------------------------------------------------------------

/// Greedily remove steps while the plan still fails with the same kind of
/// violation. Returns the shrunk plan and its violation.
///
/// # Errors
///
/// Returns an error if the driver fails.
pub fn shrink(plan: &Plan, violation: &Violation) -> Result<(Plan, Violation)> {
    let kind = violation.kind();
    let mut best = plan.clone();
    let mut best_violation = violation.clone();
    let mut i = 0;
    while i < best.steps.len() {
        let mut candidate = best.clone();
        candidate.steps.remove(i);
        // Removing a step shifts later indices; keep redaction targets valid.
        for step in &mut candidate.steps {
            if let Step::Redact { target_step, .. } = step
                && *target_step > i
            {
                *target_step -= 1;
            }
        }
        match drive(&candidate, None)? {
            Err(v) if v.kind() == kind => {
                best = candidate;
                best_violation = v;
            }
            _ => i += 1,
        }
    }
    Ok((best, best_violation))
}

/// A failing seed, with its shrunk plan.
#[derive(Debug, Clone)]
pub struct SeedFailure {
    /// Seed.
    pub seed: u64,
    /// Violation of the full plan.
    pub violation: Violation,
    /// Shrunk plan.
    pub shrunk: Plan,
    /// Violation of the shrunk plan.
    pub shrunk_violation: Violation,
}

/// Run `seeds` and return every failure, shrunk.
///
/// # Errors
///
/// Returns an error if the driver fails.
pub fn campaign(seeds: std::ops::Range<u64>, profile: Profile) -> Result<Vec<SeedFailure>> {
    let mut failures = Vec::new();
    for seed in seeds {
        let plan = generate(seed, profile);
        if let Err(violation) = drive(&plan, None)? {
            let (shrunk, shrunk_violation) = shrink(&plan, &violation)?;
            failures.push(SeedFailure {
                seed,
                violation,
                shrunk,
                shrunk_violation,
            });
        }
    }
    Ok(failures)
}
