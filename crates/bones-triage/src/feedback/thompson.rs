use anyhow::{Context, Result};
use bones_core::model::item_id::ItemId;
use chrono::{DateTime, TimeZone, Utc};
use rand::Rng;
use rand_distr::{Beta, Distribution};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::score::CompositeWeights;

const FEEDBACK_LOG_PATH: &str = ".bones/feedback.jsonl";
const AGENT_PROFILES_DIR: &str = ".bones/agent_profiles";
const PRIOR_ALPHA: f64 = 1.0;
const PRIOR_BETA: f64 = 1.0;
const CONTRIBUTION_TOLERANCE: f64 = 1e-9;

/// Posterior distribution for a single weight parameter.
///
/// Uses `Beta(alpha_param, beta_param)` with a uniform prior (`Beta(1, 1)`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct WeightPosterior {
    pub alpha_param: f64,
    pub beta_param: f64,
}

impl Default for WeightPosterior {
    fn default() -> Self {
        Self {
            alpha_param: PRIOR_ALPHA,
            beta_param: PRIOR_BETA,
        }
    }
}

impl WeightPosterior {
    fn record_success(&mut self) {
        self.alpha_param = sanitize_shape(self.alpha_param, PRIOR_ALPHA) + 1.0;
        self.beta_param = sanitize_shape(self.beta_param, PRIOR_BETA);
    }

    fn record_failure(&mut self) {
        self.alpha_param = sanitize_shape(self.alpha_param, PRIOR_ALPHA);
        self.beta_param = sanitize_shape(self.beta_param, PRIOR_BETA) + 1.0;
    }

    fn sample(&self, rng: &mut impl Rng) -> f64 {
        let alpha = sanitize_shape(self.alpha_param, PRIOR_ALPHA);
        let beta = sanitize_shape(self.beta_param, PRIOR_BETA);

        Beta::new(alpha, beta)
            .map(|distribution| distribution.sample(rng))
            .unwrap_or(0.5)
    }
}

/// Per-agent learned profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentProfile {
    pub agent_id: String,
    pub posteriors: CompositeWeights<WeightPosterior>,
}

impl AgentProfile {
    #[must_use]
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            posteriors: CompositeWeights {
                alpha: WeightPosterior::default(),
                beta: WeightPosterior::default(),
                gamma: WeightPosterior::default(),
                delta: WeightPosterior::default(),
                epsilon: WeightPosterior::default(),
            },
        }
    }
}

/// User feedback action emitted by `bn did` / `bn skip`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackAction {
    Did,
    Skip,
}

pub type FeedbackKind = FeedbackAction;

/// A stored feedback event, one JSON object per line in `.bones/feedback.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeedbackEvent {
    #[serde(with = "chrono::serde::ts_seconds", alias = "ts")]
    pub timestamp: DateTime<Utc>,
    #[serde(alias = "agent")]
    pub agent_id: String,
    #[serde(alias = "item")]
    pub item_id: ItemId,
    #[serde(alias = "type")]
    pub action: FeedbackAction,
    #[serde(default)]
    pub composite_score: f64,
    #[serde(default)]
    pub weights_used: CompositeWeights<f64>,
}

/// Legacy feedback entry format expected by the CLI integration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackEntry {
    #[serde(rename = "type")]
    pub kind: FeedbackKind,
    pub item: ItemId,
    pub agent: String,
    pub ts: u64,
}

impl FeedbackEntry {
    #[must_use]
    pub fn into_event(self, weights_used: CompositeWeights<f64>) -> FeedbackEvent {
        FeedbackEvent {
            timestamp: unix_seconds_to_datetime(self.ts),
            agent_id: self.agent,
            item_id: self.item,
            action: self.kind,
            composite_score: 0.0,
            weights_used,
        }
    }
}

impl From<FeedbackEntry> for FeedbackEvent {
    fn from(entry: FeedbackEntry) -> Self {
        entry.into_event(CompositeWeights::default())
    }
}

/// Update posterior parameters from one feedback event.
///
/// The update targets weight(s) with the highest contribution in
/// `feedback.weights_used`.
pub fn update_from_feedback(profile: &mut AgentProfile, feedback: &FeedbackEvent) {
    if profile.agent_id != feedback.agent_id {
        profile.agent_id = feedback.agent_id.clone();
    }

    let max_contribution = [
        feedback.weights_used.alpha,
        feedback.weights_used.beta,
        feedback.weights_used.gamma,
        feedback.weights_used.delta,
        feedback.weights_used.epsilon,
    ]
    .into_iter()
    .filter(|value| value.is_finite())
    .map(f64::abs)
    .fold(0.0_f64, f64::max);

    let update_all = max_contribution <= CONTRIBUTION_TOLERANCE;

    update_weight(
        &mut profile.posteriors.alpha,
        feedback.weights_used.alpha,
        feedback.action,
        max_contribution,
        update_all,
    );
    update_weight(
        &mut profile.posteriors.beta,
        feedback.weights_used.beta,
        feedback.action,
        max_contribution,
        update_all,
    );
    update_weight(
        &mut profile.posteriors.gamma,
        feedback.weights_used.gamma,
        feedback.action,
        max_contribution,
        update_all,
    );
    update_weight(
        &mut profile.posteriors.delta,
        feedback.weights_used.delta,
        feedback.action,
        max_contribution,
        update_all,
    );
    update_weight(
        &mut profile.posteriors.epsilon,
        feedback.weights_used.epsilon,
        feedback.action,
        max_contribution,
        update_all,
    );
}

/// Sample concrete weights from an agent profile.
///
/// Each component is sampled independently from its posterior and then
/// normalized so all weights sum to 1.
#[must_use]
pub fn sample_weights(profile: &AgentProfile, rng: &mut impl Rng) -> CompositeWeights<f64> {
    let sampled = CompositeWeights {
        alpha: profile.posteriors.alpha.sample(rng),
        beta: profile.posteriors.beta.sample(rng),
        gamma: profile.posteriors.gamma.sample(rng),
        delta: profile.posteriors.delta.sample(rng),
        epsilon: profile.posteriors.epsilon.sample(rng),
    };

    normalize_weights(sampled)
}

/// Append one feedback event to `.bones/feedback.jsonl`.
pub fn append_feedback_event(project_root: &Path, feedback: &FeedbackEvent) -> Result<()> {
    let log_path = feedback_log_path(project_root);

    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;

    serde_json::to_writer(&mut file, feedback)
        .with_context(|| format!("failed to serialize feedback to {}", log_path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to append newline to {}", log_path.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush {}", log_path.display()))?;

    Ok(())
}

/// Load all feedback events from `.bones/feedback.jsonl`.
pub fn load_feedback_events(project_root: &Path) -> Result<Vec<FeedbackEvent>> {
    let log_path = feedback_log_path(project_root);
    if !log_path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let reader = BufReader::new(file);

    let mut events = Vec::new();
    for (line_no, line_result) in reader.lines().enumerate() {
        let line = line_result.with_context(|| {
            format!(
                "failed reading line {} in {}",
                line_no + 1,
                log_path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }

        let event: FeedbackEvent = serde_json::from_str(&line).with_context(|| {
            format!(
                "failed parsing feedback event at {}:{}",
                log_path.display(),
                line_no + 1
            )
        })?;
        events.push(event);
    }

    Ok(events)
}

/// Load one agent profile from `.bones/agent_profiles/<agent>.json`.
///
/// If no profile exists, returns a fresh profile with uniform priors.
pub fn load_agent_profile(project_root: &Path, agent_id: &str) -> Result<AgentProfile> {
    let path = agent_profile_path(project_root, agent_id);
    if !path.exists() {
        return Ok(AgentProfile::new(agent_id));
    }

    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut profile: AgentProfile = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    if profile.agent_id.is_empty() {
        profile.agent_id = agent_id.to_string();
    }

    Ok(profile)
}

/// Persist one agent profile to `.bones/agent_profiles/<agent>.json`.
pub fn save_agent_profile(project_root: &Path, profile: &AgentProfile) -> Result<()> {
    let path = agent_profile_path(project_root, &profile.agent_id);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(profile)
        .with_context(|| format!("failed to serialize profile for {}", profile.agent_id))?;

    fs::write(&tmp_path, body)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "failed to atomically move {} to {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

/// Record a complete feedback event (append log + update profile).
pub fn record_feedback_event(project_root: &Path, feedback: FeedbackEvent) -> Result<()> {
    append_feedback_event(project_root, &feedback)?;

    let mut profile = load_agent_profile(project_root, &feedback.agent_id)?;
    update_from_feedback(&mut profile, &feedback);
    save_agent_profile(project_root, &profile)
}

/// Record feedback using the legacy `(kind, item, agent, ts)` shape.
///
/// This function resolves project root from the current directory.
pub fn record_feedback(
    kind: FeedbackKind,
    item_id: ItemId,
    agent: impl Into<String>,
    ts: u64,
) -> Result<()> {
    let project_root = std::env::current_dir().context("failed to resolve current directory")?;
    let entry = FeedbackEntry {
        kind,
        item: item_id,
        agent: agent.into(),
        ts,
    };

    record_feedback_at(&project_root, entry)
}

/// Record feedback using the legacy `(kind, item, agent, ts)` shape at a
/// caller-provided project root.
pub fn record_feedback_at(project_root: &Path, entry: FeedbackEntry) -> Result<()> {
    let mut profile = load_agent_profile(project_root, &entry.agent)?;
    let mut rng = rand::thread_rng();
    let sampled_weights = sample_weights(&profile, &mut rng);
    let event = entry.into_event(sampled_weights);

    append_feedback_event(project_root, &event)?;
    update_from_feedback(&mut profile, &event);
    save_agent_profile(project_root, &profile)
}

fn update_weight(
    posterior: &mut WeightPosterior,
    contribution: f64,
    action: FeedbackAction,
    max_contribution: f64,
    update_all: bool,
) {
    let relevant = update_all
        || (contribution.is_finite()
            && (contribution.abs() - max_contribution).abs() <= CONTRIBUTION_TOLERANCE);

    if !relevant {
        return;
    }

    match action {
        FeedbackAction::Did => posterior.record_success(),
        FeedbackAction::Skip => posterior.record_failure(),
    }
}

fn normalize_weights(weights: CompositeWeights<f64>) -> CompositeWeights<f64> {
    let alpha = sanitize_sample(weights.alpha);
    let beta = sanitize_sample(weights.beta);
    let gamma = sanitize_sample(weights.gamma);
    let delta = sanitize_sample(weights.delta);
    let epsilon = sanitize_sample(weights.epsilon);

    let total = alpha + beta + gamma + delta + epsilon;
    if total <= f64::EPSILON {
        return CompositeWeights::default();
    }

    CompositeWeights {
        alpha: alpha / total,
        beta: beta / total,
        gamma: gamma / total,
        delta: delta / total,
        epsilon: epsilon / total,
    }
}

fn sanitize_shape(value: f64, default: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        default
    }
}

fn sanitize_sample(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn feedback_log_path(project_root: &Path) -> PathBuf {
    project_root.join(FEEDBACK_LOG_PATH)
}

fn agent_profile_path(project_root: &Path, agent_id: &str) -> PathBuf {
    project_root
        .join(AGENT_PROFILES_DIR)
        .join(format!("{}.json", encode_agent_id(agent_id)))
}

fn encode_agent_id(agent_id: &str) -> String {
    let mut encoded = String::with_capacity(agent_id.len());

    for byte in agent_id.bytes() {
        let is_safe = byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.';
        if is_safe {
            encoded.push(char::from(byte));
        } else {
            push_percent_encoded_byte(&mut encoded, byte);
        }
    }

    encoded
}

fn push_percent_encoded_byte(buffer: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    buffer.push('%');
    buffer.push(char::from(HEX[(byte >> 4) as usize]));
    buffer.push(char::from(HEX[(byte & 0x0F) as usize]));
}

fn unix_seconds_to_datetime(seconds: u64) -> DateTime<Utc> {
    let seconds = if seconds > i64::MAX as u64 {
        i64::MAX
    } else {
        seconds as i64
    };

    Utc.timestamp_opt(seconds, 0)
        .single()
        .unwrap_or_else(unix_epoch)
}

fn unix_epoch() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).single().unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    fn assert_approx_eq(actual: f64, expected: f64) {
        let tolerance = 1e-10;
        assert!(
            (actual - expected).abs() <= tolerance,
            "actual ({actual}) != expected ({expected})"
        );
    }

    fn sample_feedback(
        weights_used: CompositeWeights<f64>,
        action: FeedbackAction,
    ) -> FeedbackEvent {
        FeedbackEvent {
            timestamp: unix_seconds_to_datetime(1_700_000_000),
            agent_id: "alice".to_string(),
            item_id: ItemId::parse("bn-a7x").expect("valid item id"),
            action,
            composite_score: 0.77,
            weights_used,
        }
    }

    #[test]
    fn update_from_feedback_did_updates_top_contributor() {
        let mut profile = AgentProfile::new("alice");
        let feedback = sample_feedback(
            CompositeWeights {
                alpha: 0.1,
                beta: 0.2,
                gamma: 0.9,
                delta: 0.3,
                epsilon: 0.4,
            },
            FeedbackAction::Did,
        );

        update_from_feedback(&mut profile, &feedback);

        assert_eq!(profile.posteriors.gamma.alpha_param, 2.0);
        assert_eq!(profile.posteriors.gamma.beta_param, 1.0);

        assert_eq!(profile.posteriors.alpha.alpha_param, 1.0);
        assert_eq!(profile.posteriors.beta.alpha_param, 1.0);
        assert_eq!(profile.posteriors.delta.alpha_param, 1.0);
        assert_eq!(profile.posteriors.epsilon.alpha_param, 1.0);
    }

    #[test]
    fn update_from_feedback_skip_updates_top_ties() {
        let mut profile = AgentProfile::new("alice");
        let feedback = sample_feedback(
            CompositeWeights {
                alpha: 0.8,
                beta: 0.8,
                gamma: 0.2,
                delta: 0.1,
                epsilon: 0.0,
            },
            FeedbackAction::Skip,
        );

        update_from_feedback(&mut profile, &feedback);

        assert_eq!(profile.posteriors.alpha.beta_param, 2.0);
        assert_eq!(profile.posteriors.beta.beta_param, 2.0);

        assert_eq!(profile.posteriors.gamma.beta_param, 1.0);
        assert_eq!(profile.posteriors.delta.beta_param, 1.0);
        assert_eq!(profile.posteriors.epsilon.beta_param, 1.0);
    }

    #[test]
    fn sample_weights_normalizes_to_one() {
        let profile = AgentProfile::new("alice");
        let mut rng = StdRng::seed_from_u64(42);

        let sampled = sample_weights(&profile, &mut rng);
        let total = sampled.alpha + sampled.beta + sampled.gamma + sampled.delta + sampled.epsilon;

        assert_approx_eq(total, 1.0);

        for value in [
            sampled.alpha,
            sampled.beta,
            sampled.gamma,
            sampled.delta,
            sampled.epsilon,
        ] {
            assert!(value.is_finite());
            assert!(value >= 0.0);
            assert!(value <= 1.0);
        }
    }

    #[test]
    fn load_feedback_events_parses_legacy_shape() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let bones_dir = temp.path().join(".bones");
        fs::create_dir_all(&bones_dir).expect("bones dir should exist");

        let log_path = bones_dir.join("feedback.jsonl");
        fs::write(
            &log_path,
            "{\"type\":\"did\",\"item\":\"bn-a7x\",\"agent\":\"alice\",\"ts\":1700000000}\n",
        )
        .expect("legacy feedback line should be written");

        let events = load_feedback_events(temp.path()).expect("load should succeed");

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.agent_id, "alice");
        assert_eq!(event.item_id.as_str(), "bn-a7x");
        assert_eq!(event.action, FeedbackAction::Did);
        assert_eq!(event.composite_score, 0.0);
        assert_eq!(event.weights_used, CompositeWeights::default());
        assert_eq!(event.timestamp.timestamp(), 1_700_000_000);
    }

    #[test]
    fn record_feedback_event_persists_log_and_profile() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let feedback = FeedbackEvent {
            timestamp: unix_seconds_to_datetime(1_700_000_001),
            agent_id: "bones-dev/0/red-tide".to_string(),
            item_id: ItemId::parse("bn-a7x").expect("valid item id"),
            action: FeedbackAction::Did,
            composite_score: 0.9,
            weights_used: CompositeWeights {
                alpha: 0.2,
                beta: 0.1,
                gamma: 0.3,
                delta: 0.95,
                epsilon: 0.4,
            },
        };

        record_feedback_event(temp.path(), feedback.clone()).expect("record should succeed");

        let events = load_feedback_events(temp.path()).expect("load should succeed");
        assert_eq!(events, vec![feedback]);

        let profile =
            load_agent_profile(temp.path(), "bones-dev/0/red-tide").expect("profile should load");
        assert_eq!(profile.posteriors.delta.alpha_param, 2.0);
        assert_eq!(profile.posteriors.delta.beta_param, 1.0);

        let encoded_path = agent_profile_path(temp.path(), "bones-dev/0/red-tide");
        assert!(encoded_path.exists());
        assert!(encoded_path.to_string_lossy().contains("%2F"));
    }

    #[test]
    fn record_feedback_at_samples_weights_and_updates_profile() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let entry = FeedbackEntry {
            kind: FeedbackAction::Skip,
            item: ItemId::parse("bn-a7x").expect("valid item id"),
            agent: "alice".to_string(),
            ts: 1_700_000_002,
        };

        record_feedback_at(temp.path(), entry).expect("record should succeed");

        let profile = load_agent_profile(temp.path(), "alice").expect("profile should load");
        let updated = [
            profile.posteriors.alpha.beta_param,
            profile.posteriors.beta.beta_param,
            profile.posteriors.gamma.beta_param,
            profile.posteriors.delta.beta_param,
            profile.posteriors.epsilon.beta_param,
        ];

        assert!(updated.iter().any(|value| *value > 1.0));

        let events = load_feedback_events(temp.path()).expect("events should load");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].agent_id, "alice");
        assert_eq!(events[0].action, FeedbackAction::Skip);
    }
}
