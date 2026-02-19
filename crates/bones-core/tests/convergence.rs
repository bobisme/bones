use bones_core::clock::itc::Stamp;
use bones_core::crdt::item_state::WorkItemState;
use bones_core::crdt::state::Phase;
use bones_core::event::Event;
use bones_core::event::data::{CommentData, CreateData, EventData, LinkData, MoveData, UpdateData};
use bones_core::event::types::EventType;
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;
use std::collections::{BTreeMap, BTreeSet};

fn make_event(
    event_type: EventType,
    data: EventData,
    wall_ts_us: i64,
    agent: &str,
    event_hash: &str,
    item_id: &str,
) -> Event {
    let mut stamp = Stamp::seed();
    stamp.event();
    Event {
        wall_ts_us,
        agent: agent.to_string(),
        itc: stamp.to_string(),
        parents: vec![],
        event_type,
        item_id: ItemId::new_unchecked(item_id),
        data,
        event_hash: event_hash.to_string(),
    }
}

fn create_event(item_id: &str, title: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
    make_event(
        EventType::Create,
        EventData::Create(CreateData {
            title: title.to_string(),
            kind: Kind::Task,
            size: Some(Size::M),
            urgency: Urgency::Default,
            labels: vec![],
            parent: None,
            causation: None,
            description: Some("created".to_string()),
            extra: BTreeMap::new(),
        }),
        wall_ts,
        agent,
        hash,
        item_id,
    )
}

fn update_title_event(item_id: &str, title: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
    make_event(
        EventType::Update,
        EventData::Update(UpdateData {
            field: "title".to_string(),
            value: serde_json::Value::String(title.to_string()),
            extra: BTreeMap::new(),
        }),
        wall_ts,
        agent,
        hash,
        item_id,
    )
}

fn label_event(
    item_id: &str,
    action: &str,
    label: &str,
    wall_ts: i64,
    agent: &str,
    hash: &str,
) -> Event {
    make_event(
        EventType::Update,
        EventData::Update(UpdateData {
            field: "labels".to_string(),
            value: serde_json::json!({"action": action, "label": label}),
            extra: BTreeMap::new(),
        }),
        wall_ts,
        agent,
        hash,
        item_id,
    )
}

fn move_event(item_id: &str, state: State, wall_ts: i64, agent: &str, hash: &str) -> Event {
    make_event(
        EventType::Move,
        EventData::Move(MoveData {
            state,
            reason: None,
            extra: BTreeMap::new(),
        }),
        wall_ts,
        agent,
        hash,
        item_id,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StateSummary {
    title: String,
    description: String,
    kind: Kind,
    size: Option<Size>,
    urgency: Urgency,
    parent: String,
    epoch: u64,
    phase: Phase,
    assignees: BTreeSet<String>,
    labels: BTreeSet<String>,
    blocked_by: BTreeSet<String>,
    related_to: BTreeSet<String>,
    comments: BTreeSet<String>,
    deleted: bool,
    created_at: u64,
    updated_at: u64,
}

fn summarize(state: &WorkItemState) -> StateSummary {
    StateSummary {
        title: state.title.value.clone(),
        description: state.description.value.clone(),
        kind: state.kind.value,
        size: state.size.value,
        urgency: state.urgency.value,
        parent: state.parent.value.clone(),
        epoch: state.epoch(),
        phase: state.phase(),
        assignees: state.assignee_names().into_iter().cloned().collect(),
        labels: state.label_names().into_iter().cloned().collect(),
        blocked_by: state.blocked_by_ids().into_iter().cloned().collect(),
        related_to: state.related_to_ids().into_iter().cloned().collect(),
        comments: state.comment_hashes().iter().cloned().collect(),
        deleted: state.is_deleted(),
        created_at: state.created_at,
        updated_at: state.updated_at,
    }
}

#[test]
fn three_agents_converge_all_merge_orderings() {
    let item = "bn-conv-1";

    let mut a = WorkItemState::new();
    a.apply_event(&create_event(
        item,
        "initial",
        1_000,
        "alpha",
        "blake3:a-create",
    ));
    a.apply_event(&label_event(
        item,
        "add",
        "backend",
        1_100,
        "alpha",
        "blake3:a-label",
    ));

    let mut b = WorkItemState::new();
    b.apply_event(&update_title_event(
        item,
        "title-from-bravo",
        2_000,
        "bravo",
        "blake3:b-title",
    ));
    b.apply_event(&label_event(
        item,
        "add",
        "urgent",
        2_100,
        "bravo",
        "blake3:b-label",
    ));
    b.apply_event(&make_event(
        EventType::Link,
        EventData::Link(LinkData {
            target: "bn-upstream".to_string(),
            link_type: "blocked_by".to_string(),
            extra: BTreeMap::new(),
        }),
        2_200,
        "bravo",
        "blake3:b-link",
        item,
    ));

    let mut c = WorkItemState::new();
    c.apply_event(&move_event(
        item,
        State::Doing,
        3_000,
        "charlie",
        "blake3:c-doing",
    ));
    c.apply_event(&move_event(
        item,
        State::Done,
        3_100,
        "charlie",
        "blake3:c-done",
    ));
    c.apply_event(&make_event(
        EventType::Comment,
        EventData::Comment(CommentData {
            body: "done".to_string(),
            extra: BTreeMap::new(),
        }),
        3_200,
        "charlie",
        "blake3:c-comment",
        item,
    ));

    let orderings = [
        [&a, &b, &c],
        [&a, &c, &b],
        [&b, &a, &c],
        [&b, &c, &a],
        [&c, &a, &b],
        [&c, &b, &a],
    ];

    let mut summaries = Vec::new();
    for ordering in orderings {
        let mut merged = WorkItemState::new();
        for part in ordering {
            merged.merge(part);
        }
        summaries.push(summarize(&merged));
    }

    for idx in 1..summaries.len() {
        assert_eq!(
            summaries[0], summaries[idx],
            "merge-order divergence between baseline and ordering index {idx}"
        );
    }
}

#[test]
fn lww_tie_converges_deterministically() {
    let item = "bn-conv-lww";
    let wall_ts = 50_000;

    let mut left = WorkItemState::new();
    left.apply_event(&update_title_event(
        item,
        "title-from-alpha",
        wall_ts,
        "alpha",
        "blake3:alpha-title",
    ));

    let mut right = WorkItemState::new();
    right.apply_event(&update_title_event(
        item,
        "title-from-bravo",
        wall_ts,
        "bravo",
        "blake3:bravo-title",
    ));

    let mut left_then_right = left.clone();
    left_then_right.merge(&right);

    let mut right_then_left = right.clone();
    right_then_left.merge(&left);

    assert_eq!(left_then_right.title.value, right_then_left.title.value);
    assert_eq!(left_then_right.title.value, "title-from-bravo");
}

#[test]
fn orset_add_remove_race_is_add_wins_and_convergent() {
    let item = "bn-conv-orset";

    let mut add_side = WorkItemState::new();
    add_side.apply_event(&label_event(
        item,
        "add",
        "sre",
        1_000,
        "alpha",
        "blake3:add-label",
    ));

    let mut remove_side = WorkItemState::new();
    remove_side.apply_event(&label_event(
        item,
        "remove",
        "sre",
        1_000,
        "bravo",
        "blake3:remove-label",
    ));

    let mut add_then_remove = add_side.clone();
    add_then_remove.merge(&remove_side);

    let mut remove_then_add = remove_side.clone();
    remove_then_add.merge(&add_side);

    let add_then_remove_labels: BTreeSet<String> =
        add_then_remove.label_names().into_iter().cloned().collect();
    let remove_then_add_labels: BTreeSet<String> =
        remove_then_add.label_names().into_iter().cloned().collect();

    assert_eq!(add_then_remove_labels, remove_then_add_labels);
    assert!(add_then_remove_labels.contains("sre"));
}

#[test]
fn epoch_phase_race_converges_to_higher_epoch() {
    let item = "bn-conv-epoch";

    let mut base = WorkItemState::new();
    base.apply_event(&move_event(
        item,
        State::Done,
        1_000,
        "alpha",
        "blake3:base-done",
    ));

    let mut reopen_branch = base.clone();
    reopen_branch.apply_event(&move_event(
        item,
        State::Open,
        2_000,
        "alpha",
        "blake3:reopen",
    ));

    let mut old_epoch_branch = base.clone();
    old_epoch_branch.apply_event(&move_event(
        item,
        State::Archived,
        2_000,
        "bravo",
        "blake3:archive-old-epoch",
    ));

    let mut left = reopen_branch.clone();
    left.merge(&old_epoch_branch);

    let mut right = old_epoch_branch.clone();
    right.merge(&reopen_branch);

    assert_eq!(left.epoch(), right.epoch());
    assert_eq!(left.phase(), right.phase());
    assert_eq!(left.epoch(), 1);
    assert_eq!(left.phase(), Phase::Open);
}
