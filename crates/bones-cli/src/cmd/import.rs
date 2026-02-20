use anyhow::{Context as _, Result};
use bones_core::event::{
    AssignAction, AssignData, CommentData, CreateData, Event, EventData, EventType, MoveData,
    PartialParsedLine, parse_line_partial, writer::write_event,
};
use bones_core::model::item::{Kind, State, Urgency};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use chrono::Datelike;
use clap::Args;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Import from a GitHub repository (<owner>/<repo>).
    #[arg(long, value_name = "OWNER/REPO", conflicts_with = "jsonl")]
    pub github: Option<String>,

    /// Import events from a JSONL input stream.
    #[arg(long)]
    pub jsonl: bool,

    /// Optional path for JSONL import; omit to read from stdin.
    #[arg(long, value_name = "PATH")]
    pub input: Option<PathBuf>,

    /// GitHub API token (optional). Falls back to GITHUB_TOKEN env var.
    #[arg(long)]
    pub token: Option<String>,

    /// Output report in JSON format.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Default, Serialize)]
struct ImportReport {
    repo: String,
    fetched_issues: usize,
    imported_issues: usize,
    imported_milestones: usize,
    imported_comments: usize,
    imported_assignments: usize,
    skipped_existing: usize,
    api_requests: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoSlug {
    owner: String,
    repo: String,
}

impl RepoSlug {
    fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let Some((owner, repo)) = trimmed.split_once('/') else {
            anyhow::bail!("invalid repo slug '{trimmed}': expected <owner>/<repo>");
        };

        if owner.is_empty() || repo.is_empty() {
            anyhow::bail!("invalid repo slug '{trimmed}': expected <owner>/<repo>");
        }

        Ok(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }

    fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    #[serde(default)]
    labels: Vec<GitHubLabel>,
    #[serde(default)]
    milestone: Option<GitHubMilestone>,
    #[serde(default)]
    assignees: Vec<GitHubUser>,
    #[serde(default)]
    comments: u64,
    comments_url: String,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    closed_at: Option<String>,
    user: GitHubUser,
    html_url: String,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubLabel {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubMilestone {
    number: u64,
    title: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubComment {
    #[serde(default)]
    body: String,
    created_at: String,
    user: GitHubUser,
}

#[derive(Debug, Clone)]
struct MilestonePlan {
    milestone: GitHubMilestone,
    first_seen_ts: i64,
}

#[derive(Debug, Clone)]
enum PlannedPayload {
    Create(CreateData),
    Assign(AssignData),
    Comment(CommentData),
    Move(MoveData),
}

#[derive(Debug, Clone)]
struct PlannedEvent {
    ts: i64,
    order: u8,
    agent: String,
    payload: PlannedPayload,
}

struct GitHubClient {
    token: Option<String>,
    requests: Cell<usize>,
}

impl GitHubClient {
    fn new(token: Option<String>) -> Self {
        Self {
            token,
            requests: Cell::new(0),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.get()
    }

    fn fetch_issues(&self, repo: &RepoSlug) -> Result<Vec<GitHubIssue>> {
        let mut issues = Vec::new();
        let mut page = 1_u32;

        loop {
            let url = format!(
                "https://api.github.com/repos/{}/{}/issues?state=all&per_page=100&page={page}&sort=created&direction=asc",
                repo.owner, repo.repo
            );

            let batch: Vec<GitHubIssue> = self
                .get_json(&url)
                .with_context(|| format!("failed to fetch issues page {page}"))?;

            if batch.is_empty() {
                break;
            }

            let raw_len = batch.len();
            issues.extend(
                batch
                    .into_iter()
                    .filter(|issue| issue.pull_request.is_none()),
            );

            if raw_len < 100 {
                break;
            }

            page += 1;
        }

        Ok(issues)
    }

    fn fetch_comments(&self, comments_url: &str) -> Result<Vec<GitHubComment>> {
        let mut comments = Vec::new();
        let mut page = 1_u32;

        loop {
            let url = paged_url(comments_url, page);
            let batch: Vec<GitHubComment> = self
                .get_json(&url)
                .with_context(|| format!("failed to fetch comments page {page}"))?;

            if batch.is_empty() {
                break;
            }

            let raw_len = batch.len();
            comments.extend(batch);

            if raw_len < 100 {
                break;
            }

            page += 1;
        }

        Ok(comments)
    }

    fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.requests.set(self.requests.get() + 1);

        let mut request = ureq::get(url)
            .set("Accept", "application/vnd.github+json")
            .set("User-Agent", "bones-cli");

        if let Some(token) = &self.token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }

        let response = request
            .call()
            .map_err(|err| anyhow::anyhow!("GitHub API request failed for {url}: {err}"))?;

        response
            .into_json::<T>()
            .context("failed to decode GitHub API JSON response")
    }
}

pub fn run_import(args: &ImportArgs, project_root: &Path) -> Result<()> {
    if args.jsonl {
        return run_jsonl_import(args, project_root);
    }

    let Some(github) = args.github.as_deref() else {
        anyhow::bail!("missing required flag: --github <owner>/<repo> or --jsonl");
    };

    let repo = RepoSlug::parse(github)?;
    let token = args
        .token
        .clone()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());

    let client = GitHubClient::new(token);
    let mut issues = client.fetch_issues(&repo)?;
    issues.sort_by_key(|issue| issue.number);

    let shard_manager = ShardManager::new(project_root.join(".bones"));
    let active_shard = shard_manager
        .init()
        .context("failed to initialize .bones shard state")?;

    let mut existing_item_ids = collect_existing_item_ids(&shard_manager)?;
    let mut report = ImportReport {
        repo: repo.full_name(),
        fetched_issues: issues.len(),
        ..ImportReport::default()
    };

    let milestones = build_milestone_plan(&issues)?;
    let mut milestone_ids: HashMap<u64, ItemId> = HashMap::new();

    for plan in milestones.values() {
        let milestone_id = milestone_item_id(&repo, plan.milestone.number)?;
        milestone_ids.insert(plan.milestone.number, milestone_id.clone());

        if existing_item_ids.contains(milestone_id.as_str()) {
            report.skipped_existing += 1;
            continue;
        }

        let mut extra = BTreeMap::new();
        extra.insert("source".to_string(), json!("github"));
        extra.insert("repo".to_string(), json!(repo.full_name()));
        extra.insert("milestone_number".to_string(), json!(plan.milestone.number));

        let mut event = Event {
            wall_ts_us: plan.first_seen_ts,
            agent: "github/importer".to_string(),
            itc: format!("itc:github:{}", plan.milestone.number),
            parents: Vec::new(),
            event_type: EventType::Create,
            item_id: milestone_id,
            data: EventData::Create(CreateData {
                title: format!("Milestone: {}", plan.milestone.title),
                kind: Kind::Goal,
                size: None,
                urgency: Urgency::Default,
                labels: vec!["github".to_string(), "github:milestone".to_string()],
                parent: None,
                causation: None,
                description: plan.milestone.description.clone(),
                extra,
            }),
            event_hash: String::new(),
        };

        append_event(&shard_manager, active_shard, &mut event)?;
        existing_item_ids.insert(event.item_id.to_string());
        report.imported_milestones += 1;
    }

    for issue in &issues {
        let issue_id = issue_item_id(&repo, issue.number)?;

        if existing_item_ids.contains(issue_id.as_str()) {
            report.skipped_existing += 1;
            continue;
        }

        let comments = if issue.comments > 0 {
            client
                .fetch_comments(&issue.comments_url)
                .with_context(|| format!("failed to fetch comments for issue #{}", issue.number))?
        } else {
            Vec::new()
        };

        let parent = issue
            .milestone
            .as_ref()
            .and_then(|milestone| milestone_ids.get(&milestone.number));

        let planned = plan_issue_events(issue, &comments, parent)?;
        let mut previous_hash: Option<String> = None;

        for (index, planned_event) in planned.into_iter().enumerate() {
            let (event_type, data) = planned_event.payload.to_event_parts();

            let mut event = Event {
                wall_ts_us: planned_event.ts,
                agent: planned_event.agent,
                itc: format!("itc:github:{}:{}", issue.number, index),
                parents: previous_hash.iter().cloned().collect(),
                event_type,
                item_id: issue_id.clone(),
                data,
                event_hash: String::new(),
            };

            append_event(&shard_manager, active_shard, &mut event)?;
            previous_hash = Some(event.event_hash.clone());

            match event.event_type {
                EventType::Assign => report.imported_assignments += 1,
                EventType::Comment => report.imported_comments += 1,
                _ => {}
            }
        }

        existing_item_ids.insert(issue_id.to_string());
        report.imported_issues += 1;
    }

    report.api_requests = client.request_count();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct JsonlEventRecord {
    timestamp: i64,
    agent: String,
    #[serde(rename = "type")]
    event_type: String,
    item_id: String,
    data: JsonValue,
}

#[derive(Debug, Serialize)]
struct ImportSummary {
    mode: &'static str,
    input_path: Option<String>,
    total_lines: usize,
    imported: usize,
    skipped_invalid: usize,
    imported_per_type: HashMap<String, usize>,
}

fn run_jsonl_import(args: &ImportArgs, project_root: &Path) -> Result<()> {
    let input_reader: Box<dyn BufRead> = match &args.input {
        Some(path) => {
            let file = File::open(path)
                .with_context(|| format!("failed to open JSONL input {}", path.display()))?;
            Box::new(BufReader::new(file))
        }
        None => Box::new(BufReader::new(io::stdin())),
    };

    let shard_manager = ShardManager::new(project_root.join(".bones"));
    shard_manager
        .init()
        .context("failed to initialize .bones shard state")?;

    let mut parent_index: HashMap<String, String> = HashMap::new();
    let mut report = ImportSummary {
        mode: "jsonl",
        input_path: args.input.as_ref().map(|path| path.display().to_string()),
        total_lines: 0,
        imported: 0,
        skipped_invalid: 0,
        imported_per_type: HashMap::new(),
    };

    for (line_no, line) in input_reader.lines().enumerate() {
        let line_no = line_no + 1;
        let raw = line.with_context(|| format!("failed to read jsonline {line_no}"))?;
        if raw.trim().is_empty() {
            continue;
        }

        report.total_lines += 1;
        let record: JsonlEventRecord = match serde_json::from_str(&raw) {
            Ok(record) => record,
            Err(err) => {
                report.skipped_invalid += 1;
                eprintln!("skip line {line_no}: invalid JSON - {err}");
                continue;
            }
        };

        let item_id = ItemId::parse(&record.item_id)
            .with_context(|| format!("line {line_no}: invalid item_id '{}'", record.item_id))?;

        let event_type = record.event_type.parse::<EventType>().with_context(|| {
            format!("line {line_no}: unknown event type '{}'", record.event_type)
        })?;

        let data = EventData::deserialize_for(event_type, &record.data.to_string())
            .with_context(|| format!("line {line_no}: invalid data payload"))?;

        let mut event = Event {
            wall_ts_us: record.timestamp,
            agent: record.agent,
            itc: format!("itc:jsonl:{line_no}"),
            parents: parent_index
                .get(item_id.as_str())
                .cloned()
                .into_iter()
                .collect(),
            event_type,
            item_id: item_id.clone(),
            data,
            event_hash: String::new(),
        };

        let line = write_event(&mut event).context("failed to serialize imported event")?;
        let (year, month) = event_to_shard_timestamp(event.wall_ts_us)
            .with_context(|| format!("line {line_no}: invalid timestamp {}", event.wall_ts_us))?;

        shard_manager
            .create_shard(year, month)
            .context("failed to initialize shard for timestamp")?;
        shard_manager
            .append_raw(year, month, &line)
            .context("failed to append JSONL import event")?;

        parent_index.insert(item_id.to_string(), event.event_hash.clone());
        *report
            .imported_per_type
            .entry(event_type.as_str().to_string())
            .or_default() += 1;
        report.imported += 1;
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "bn import --jsonl {}",
            report.input_path.as_deref().unwrap_or("<stdin>")
        );
        println!("  total lines:     {}", report.total_lines);
        println!("  imported:       {}", report.imported);
        println!("  skipped:        {}", report.skipped_invalid);
        if !report.imported_per_type.is_empty() {
            println!("  types:");
            for (event_type, count) in &report.imported_per_type {
                println!("    {event_type}: {count}");
            }
        }
    }

    Ok(())
}

fn event_to_shard_timestamp(timestamp_us: i64) -> Result<(i32, u32)> {
    let datetime = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(timestamp_us)
        .ok_or_else(|| anyhow::anyhow!("timestamp out of range: {timestamp_us}"))?;

    Ok((datetime.year(), datetime.month()))
}

fn collect_existing_item_ids(shard_manager: &ShardManager) -> Result<HashSet<String>> {
    let replay = shard_manager
        .replay()
        .context("failed to read existing event shards")?;

    let mut ids = HashSet::new();
    for line in replay.lines() {
        if let Ok(PartialParsedLine::Event(partial)) = parse_line_partial(line) {
            ids.insert(partial.item_id_raw.to_string());
        }
    }

    Ok(ids)
}

fn build_milestone_plan(issues: &[GitHubIssue]) -> Result<BTreeMap<u64, MilestonePlan>> {
    let mut milestones = BTreeMap::new();

    for issue in issues {
        let Some(milestone) = issue.milestone.as_ref() else {
            continue;
        };

        let created_ts = parse_timestamp_us(&issue.created_at)?;

        milestones
            .entry(milestone.number)
            .and_modify(|plan: &mut MilestonePlan| {
                plan.first_seen_ts = plan.first_seen_ts.min(created_ts);
            })
            .or_insert_with(|| MilestonePlan {
                milestone: milestone.clone(),
                first_seen_ts: created_ts,
            });
    }

    Ok(milestones)
}

fn plan_issue_events(
    issue: &GitHubIssue,
    comments: &[GitHubComment],
    parent: Option<&ItemId>,
) -> Result<Vec<PlannedEvent>> {
    let created_ts = parse_timestamp_us(&issue.created_at)?;

    let labels: Vec<String> = issue
        .labels
        .iter()
        .map(|label| label.name.trim())
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    let kind = if labels.iter().any(|label| label.eq_ignore_ascii_case("bug")) {
        Kind::Bug
    } else {
        Kind::Task
    };

    let mut create_extra = BTreeMap::new();
    create_extra.insert("source".to_string(), json!("github"));
    create_extra.insert("issue_number".to_string(), json!(issue.number));
    create_extra.insert("issue_url".to_string(), json!(issue.html_url));

    let mut planned = vec![PlannedEvent {
        ts: created_ts,
        order: 0,
        agent: github_agent(&issue.user.login),
        payload: PlannedPayload::Create(CreateData {
            title: issue.title.clone(),
            kind,
            size: None,
            urgency: Urgency::Default,
            labels,
            parent: parent.map(ToString::to_string),
            causation: None,
            description: issue.body.clone(),
            extra: create_extra,
        }),
    }];

    for (index, assignee) in issue.assignees.iter().enumerate() {
        let mut assign_extra = BTreeMap::new();
        assign_extra.insert("source".to_string(), json!("github"));
        assign_extra.insert("issue_number".to_string(), json!(issue.number));

        planned.push(PlannedEvent {
            ts: created_ts + (index as i64) + 1,
            order: 1,
            agent: github_agent(&issue.user.login),
            payload: PlannedPayload::Assign(AssignData {
                agent: github_agent(&assignee.login),
                action: AssignAction::Assign,
                extra: assign_extra,
            }),
        });
    }

    for comment in comments {
        let mut comment_extra = BTreeMap::new();
        comment_extra.insert("source".to_string(), json!("github"));
        comment_extra.insert("issue_number".to_string(), json!(issue.number));
        comment_extra.insert("author".to_string(), json!(comment.user.login));

        planned.push(PlannedEvent {
            ts: parse_timestamp_us(&comment.created_at)?,
            order: 2,
            agent: github_agent(&comment.user.login),
            payload: PlannedPayload::Comment(CommentData {
                body: comment.body.clone(),
                extra: comment_extra,
            }),
        });
    }

    if issue.state.eq_ignore_ascii_case("closed") {
        let closed_ts = issue
            .closed_at
            .as_deref()
            .map(parse_timestamp_us)
            .transpose()?
            .unwrap_or_else(|| parse_timestamp_us(&issue.updated_at).unwrap_or(created_ts));

        let mut move_extra = BTreeMap::new();
        move_extra.insert("source".to_string(), json!("github"));
        move_extra.insert("issue_number".to_string(), json!(issue.number));

        planned.push(PlannedEvent {
            ts: closed_ts,
            order: 3,
            agent: "github/importer".to_string(),
            payload: PlannedPayload::Move(MoveData {
                state: State::Done,
                reason: Some("Imported closed issue from GitHub".to_string()),
                extra: move_extra,
            }),
        });
    }

    planned.sort_by_key(|event| (event.ts, event.order));
    Ok(planned)
}

fn append_event(
    shard_manager: &ShardManager,
    active_shard: (i32, u32),
    event: &mut Event,
) -> Result<()> {
    let line = write_event(event).context("failed to serialize imported event")?;
    shard_manager
        .append_raw(active_shard.0, active_shard.1, &line)
        .context("failed to append imported event")
}

fn issue_item_id(repo: &RepoSlug, issue_number: u64) -> Result<ItemId> {
    deterministic_item_id(repo, "issue", issue_number)
}

fn milestone_item_id(repo: &RepoSlug, milestone_number: u64) -> Result<ItemId> {
    deterministic_item_id(repo, "milestone", milestone_number)
}

fn deterministic_item_id(repo: &RepoSlug, kind: &str, number: u64) -> Result<ItemId> {
    let seed = format!("{}:{}:{kind}:{number}", repo.owner, repo.repo);
    let digest = blake3::hash(seed.as_bytes()).to_hex().to_string();
    let raw = format!("bn-g{number}{}", &digest[..6]);

    ItemId::parse(&raw).with_context(|| format!("generated invalid item id '{raw}'"))
}

fn github_agent(login: &str) -> String {
    let normalized = login.trim().replace(char::is_whitespace, "-");
    format!("github/{normalized}")
}

fn parse_timestamp_us(raw: &str) -> Result<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("invalid RFC3339 timestamp: {raw}"))?;
    Ok(parsed.timestamp_micros())
}

fn paged_url(base: &str, page: u32) -> String {
    if base.contains('?') {
        format!("{base}&per_page=100&page={page}")
    } else {
        format!("{base}?per_page=100&page={page}")
    }
}

fn print_report(report: &ImportReport) {
    println!("bn import --github {}", report.repo);
    println!("  fetched issues:      {}", report.fetched_issues);
    println!("  imported milestones: {}", report.imported_milestones);
    println!("  imported issues:     {}", report.imported_issues);
    println!("  imported comments:   {}", report.imported_comments);
    println!("  imported assignees:  {}", report.imported_assignments);
    println!("  skipped existing:    {}", report.skipped_existing);
    println!("  API requests:        {}", report.api_requests);
}

impl PlannedPayload {
    fn to_event_parts(self) -> (EventType, EventData) {
        match self {
            Self::Create(data) => (EventType::Create, EventData::Create(data)),
            Self::Assign(data) => (EventType::Assign, EventData::Assign(data)),
            Self::Comment(data) => (EventType::Comment, EventData::Comment(data)),
            Self::Move(data) => (EventType::Move, EventData::Move(data)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repo_slug_accepts_valid_input() {
        let parsed = RepoSlug::parse("owner/repo").expect("should parse");
        assert_eq!(parsed.owner, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.full_name(), "owner/repo");
    }

    #[test]
    fn parse_repo_slug_rejects_invalid_input() {
        assert!(RepoSlug::parse("owner").is_err());
        assert!(RepoSlug::parse("/repo").is_err());
        assert!(RepoSlug::parse("owner/").is_err());
    }

    #[test]
    fn deterministic_item_id_is_stable() {
        let repo = RepoSlug::parse("acme/widget").expect("valid repo");
        let id1 = issue_item_id(&repo, 42).expect("id");
        let id2 = issue_item_id(&repo, 42).expect("id");
        assert_eq!(id1, id2);
    }

    #[test]
    fn deterministic_item_id_differs_across_repos() {
        let repo_a = RepoSlug::parse("acme/widget").expect("valid repo");
        let repo_b = RepoSlug::parse("acme/other").expect("valid repo");
        let id_a = issue_item_id(&repo_a, 42).expect("id");
        let id_b = issue_item_id(&repo_b, 42).expect("id");
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn plan_issue_events_adds_move_for_closed_issue() {
        let issue = GitHubIssue {
            number: 7,
            title: "Fix login".to_string(),
            body: Some("Body".to_string()),
            state: "closed".to_string(),
            labels: vec![GitHubLabel {
                name: "bug".to_string(),
            }],
            milestone: None,
            assignees: vec![],
            comments: 0,
            comments_url: "https://api.github.com/comments".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: Some("2026-01-03T00:00:00Z".to_string()),
            user: GitHubUser {
                login: "alice".to_string(),
            },
            html_url: "https://github.com/acme/widget/issues/7".to_string(),
            pull_request: None,
        };

        let planned = plan_issue_events(&issue, &[], None).expect("should plan");
        assert_eq!(planned.len(), 2, "create + move expected");

        let has_move = planned
            .iter()
            .any(|event| matches!(event.payload, PlannedPayload::Move(_)));
        assert!(has_move, "closed issues should emit move->done event");
    }

    #[test]
    fn paged_url_appends_query_params() {
        assert_eq!(
            paged_url("https://api.github.com/x", 2),
            "https://api.github.com/x?per_page=100&page=2"
        );
        assert_eq!(
            paged_url("https://api.github.com/x?a=1", 3),
            "https://api.github.com/x?a=1&per_page=100&page=3"
        );
    }
}
