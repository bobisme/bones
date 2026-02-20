//! Integration tests for Prolly Tree sync protocol.
//!
//! Verifies that two diverged replicas converge after syncing via the
//! protocol module, and that sync is idempotent.

use bones_core::event::data::{CreateData, EventData, UpdateData};
use bones_core::event::{Event, EventType};
use bones_core::model::item::Kind;
use bones_core::model::item::Urgency;
use bones_core::model::item_id::ItemId;
use bones_core::sync::prolly::ProllyTree;
use bones_core::sync::protocol::sync_in_memory;
use std::collections::{BTreeMap, HashSet};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_create_event(item_id: &str, ts: i64, agent: &str) -> Event {
    Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "0:0".to_string(),
        parents: vec![],
        event_type: EventType::Create,
        item_id: ItemId::new_unchecked(item_id),
        data: EventData::Create(CreateData {
            title: format!("Item {item_id}"),
            kind: Kind::Task,
            size: None,
            urgency: Urgency::Default,
            labels: vec![],
            parent: None,
            causation: None,
            description: None,
            extra: BTreeMap::new(),
        }),
        event_hash: format!("blake3:{item_id}_{ts}_{agent}"),
    }
}

fn make_update_event(item_id: &str, ts: i64, agent: &str, field: &str, value: &str) -> Event {
    Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "0:0".to_string(),
        parents: vec![],
        event_type: EventType::Update,
        item_id: ItemId::new_unchecked(item_id),
        data: EventData::Update(UpdateData {
            field: field.to_string(),
            value: serde_json::Value::String(value.to_string()),
            extra: BTreeMap::new(),
        }),
        event_hash: format!("blake3:{item_id}_{ts}_{agent}_{field}"),
    }
}

fn all_hashes(events: &[Event]) -> HashSet<String> {
    events.iter().map(|e| e.event_hash.clone()).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two repos with diverged events sync via Prolly Tree and converge.
#[test]
fn two_repos_sync_and_converge() {
    // Shared history: items created by both repos.
    let shared = vec![
        make_create_event("item-1", 1000, "agent-shared"),
        make_create_event("item-2", 2000, "agent-shared"),
        make_create_event("item-3", 3000, "agent-shared"),
    ];

    // Repo A adds unique events.
    let mut repo_a = shared.clone();
    repo_a.push(make_update_event("item-1", 4000, "agent-a", "status", "doing"));
    repo_a.push(make_create_event("item-a1", 5000, "agent-a"));

    // Repo B adds different unique events.
    let mut repo_b = shared.clone();
    repo_b.push(make_update_event("item-2", 4500, "agent-b", "status", "done"));
    repo_b.push(make_create_event("item-b1", 5500, "agent-b"));

    // Sync.
    let result = sync_in_memory(&repo_a, &repo_b).unwrap();

    // A should receive B's unique events.
    assert_eq!(result.local_received.len(), 2);
    let a_received_hashes: HashSet<_> = result
        .local_received
        .iter()
        .map(|e| e.event_hash.as_str())
        .collect();
    assert!(a_received_hashes.contains("blake3:item-2_4500_agent-b_status"));
    assert!(a_received_hashes.contains("blake3:item-b1_5500_agent-b"));

    // B should receive A's unique events.
    assert_eq!(result.remote_received.len(), 2);
    let b_received_hashes: HashSet<_> = result
        .remote_received
        .iter()
        .map(|e| e.event_hash.as_str())
        .collect();
    assert!(b_received_hashes.contains("blake3:item-1_4000_agent-a_status"));
    assert!(b_received_hashes.contains("blake3:item-a1_5000_agent-a"));

    // After merging received events, both repos have the same event set.
    let mut a_merged = repo_a.clone();
    a_merged.extend(result.local_received);
    let mut b_merged = repo_b.clone();
    b_merged.extend(result.remote_received);

    assert_eq!(all_hashes(&a_merged), all_hashes(&b_merged));

    // Prolly Trees should now produce identical root hashes.
    let tree_a = ProllyTree::build(&a_merged);
    let tree_b = ProllyTree::build(&b_merged);
    assert_eq!(tree_a.root.hash(), tree_b.root.hash());
}

/// Sync is idempotent: syncing twice produces the same result.
#[test]
fn sync_is_idempotent() {
    let mut repo_a = vec![
        make_create_event("x", 100, "a"),
        make_update_event("x", 200, "a", "title", "Updated by A"),
    ];
    let mut repo_b = vec![
        make_create_event("x", 100, "a"), // shared create
        make_update_event("x", 300, "b", "title", "Updated by B"),
    ];

    // First sync.
    let r1 = sync_in_memory(&repo_a, &repo_b).unwrap();
    repo_a.extend(r1.local_received);
    repo_b.extend(r1.remote_received);

    // Verify convergence.
    let tree_a = ProllyTree::build(&repo_a);
    let tree_b = ProllyTree::build(&repo_b);
    assert_eq!(tree_a.root.hash(), tree_b.root.hash());

    // Second sync — should be a no-op.
    let r2 = sync_in_memory(&repo_a, &repo_b).unwrap();
    assert!(r2.local_report.is_noop());
    assert!(r2.remote_report.is_noop());
    assert_eq!(r2.local_report.rounds, 1); // fast path
}

/// Sync with empty repo: non-empty repo syncs to empty, empty gets all events.
#[test]
fn sync_to_empty_repo() {
    let populated = vec![
        make_create_event("a", 1, "agent"),
        make_create_event("b", 2, "agent"),
        make_create_event("c", 3, "agent"),
    ];

    let result = sync_in_memory(&[], &populated).unwrap();
    assert_eq!(result.local_received.len(), 3);
    assert!(result.remote_received.is_empty());

    // Reverse direction.
    let result2 = sync_in_memory(&populated, &[]).unwrap();
    assert!(result2.local_received.is_empty());
    assert_eq!(result2.remote_received.len(), 3);
}

/// Sync with identical repos: no events transferred, no state change.
#[test]
fn sync_identical_repos_is_noop() {
    let events = vec![
        make_create_event("item-1", 100, "agent"),
        make_create_event("item-2", 200, "agent"),
    ];

    let result = sync_in_memory(&events, &events).unwrap();
    assert!(result.local_report.is_noop());
    assert!(result.remote_report.is_noop());
    assert_eq!(result.local_report.rounds, 1);
    assert_eq!(result.local_report.bytes_transferred, 0);
}

/// Sync handles concurrent events on same item.
#[test]
fn sync_concurrent_same_item() {
    // Both agents create events for item-1 independently.
    let repo_a = vec![
        make_create_event("item-1", 100, "agent-a"),
        make_update_event("item-1", 200, "agent-a", "status", "doing"),
    ];
    let repo_b = vec![
        make_create_event("item-1", 150, "agent-b"),
        make_update_event("item-1", 250, "agent-b", "status", "done"),
    ];

    let result = sync_in_memory(&repo_a, &repo_b).unwrap();

    // Each side should receive the other's events.
    assert_eq!(result.local_received.len(), 2);
    assert_eq!(result.remote_received.len(), 2);

    // After merge, both sides have all 4 events.
    let mut a_merged = repo_a;
    a_merged.extend(result.local_received);
    let mut b_merged = repo_b;
    b_merged.extend(result.remote_received);

    assert_eq!(all_hashes(&a_merged).len(), 4);
    assert_eq!(all_hashes(&a_merged), all_hashes(&b_merged));
}

/// Verify sync report contains meaningful data.
#[test]
fn sync_report_has_correct_counts() {
    let a = vec![
        make_create_event("shared", 1, "x"),
        make_create_event("a-only-1", 2, "a"),
        make_create_event("a-only-2", 3, "a"),
    ];
    let b = vec![
        make_create_event("shared", 1, "x"),
        make_create_event("b-only-1", 4, "b"),
    ];

    let result = sync_in_memory(&a, &b).unwrap();
    assert_eq!(result.local_report.events_sent, 2); // a-only-1, a-only-2
    assert_eq!(result.local_report.events_received, 1); // b-only-1
    assert_eq!(result.remote_report.events_sent, 1);
    assert_eq!(result.remote_report.events_received, 2);
    assert!(result.local_report.bytes_transferred > 0);
    assert_eq!(result.local_report.rounds, 3); // full protocol
}

/// Large-scale sync: 500 shared + 100 diverged on each side.
#[test]
fn sync_large_scale_convergence() {
    let shared: Vec<Event> = (0..500)
        .map(|i| make_create_event(&format!("s{i:04}"), i, "shared"))
        .collect();

    let mut a = shared.clone();
    for i in 0..100 {
        a.push(make_create_event(
            &format!("a{i:03}"),
            1000 + i,
            "agent-a",
        ));
    }

    let mut b = shared;
    for i in 0..100 {
        b.push(make_create_event(
            &format!("b{i:03}"),
            2000 + i,
            "agent-b",
        ));
    }

    let result = sync_in_memory(&a, &b).unwrap();
    assert_eq!(result.local_received.len(), 100);
    assert_eq!(result.remote_received.len(), 100);

    // Verify convergence.
    let mut a_merged = a;
    a_merged.extend(result.local_received);
    let mut b_merged = b;
    b_merged.extend(result.remote_received);

    let tree_a = ProllyTree::build(&a_merged);
    let tree_b = ProllyTree::build(&b_merged);
    assert_eq!(tree_a.root.hash(), tree_b.root.hash());
    assert_eq!(tree_a.event_count, 700); // 500 + 100 + 100
}

/// Multi-round sync: three repos converge after pairwise syncs.
#[test]
fn three_way_pairwise_convergence() {
    let base = vec![make_create_event("base", 1, "origin")];

    let mut a = base.clone();
    a.push(make_create_event("a-item", 10, "agent-a"));

    let mut b = base.clone();
    b.push(make_create_event("b-item", 20, "agent-b"));

    let mut c = base;
    c.push(make_create_event("c-item", 30, "agent-c"));

    // Sync A <-> B.
    let r_ab = sync_in_memory(&a, &b).unwrap();
    a.extend(r_ab.local_received);
    b.extend(r_ab.remote_received);

    // Sync B <-> C (B now has A's events too).
    let r_bc = sync_in_memory(&b, &c).unwrap();
    b.extend(r_bc.local_received);
    c.extend(r_bc.remote_received);

    // Sync A <-> C (C now has B's events too).
    let r_ac = sync_in_memory(&a, &c).unwrap();
    a.extend(r_ac.local_received);
    c.extend(r_ac.remote_received);

    // All three should have the same event set.
    let hashes_a = all_hashes(&a);
    let hashes_b = all_hashes(&b);
    let hashes_c = all_hashes(&c);
    assert_eq!(hashes_a, hashes_b);
    assert_eq!(hashes_b, hashes_c);
    assert_eq!(hashes_a.len(), 4); // base + a + b + c
}
