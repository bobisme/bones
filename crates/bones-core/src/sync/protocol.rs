//! Prolly Tree sync protocol for non-git event replication.
//!
//! Two replicas exchange Prolly Tree root hashes, identify divergent subtrees
//! in O(log N), and transfer only the missing events.
//!
//! The protocol is transport-agnostic: any type implementing [`SyncTransport`]
//! can be used (TCP, HTTP, MCP, USB drive via file exchange, etc.).

use std::collections::{HashMap, HashSet};

use crate::event::Event;
use crate::sync::prolly::{Hash, ProllyTree};

// ---------------------------------------------------------------------------
// Transport trait
// ---------------------------------------------------------------------------

/// Abstraction over the wire protocol.
///
/// Implementations shuttle hashes and events between two replicas.
/// The trait is intentionally simple; higher-level protocols (compression,
/// batching, authentication) are layered on top.
pub trait SyncTransport {
    /// Error type for transport operations.
    type Error: std::fmt::Debug + std::fmt::Display;

    /// Send a root hash to the remote.
    fn send_hash(&mut self, hash: &Hash) -> Result<(), Self::Error>;

    /// Receive a root hash from the remote.
    fn recv_hash(&mut self) -> Result<Hash, Self::Error>;

    /// Send a list of event hashes that we want the remote to check.
    fn send_event_hashes(&mut self, hashes: &[String]) -> Result<(), Self::Error>;

    /// Receive a list of event hashes from the remote.
    fn recv_event_hashes(&mut self) -> Result<Vec<String>, Self::Error>;

    /// Send events to the remote.
    fn send_events(&mut self, events: &[Event]) -> Result<(), Self::Error>;

    /// Receive events from the remote.
    fn recv_events(&mut self) -> Result<Vec<Event>, Self::Error>;
}

// ---------------------------------------------------------------------------
// Sync report
// ---------------------------------------------------------------------------

/// Summary of a completed sync operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Number of events sent to the remote.
    pub events_sent: usize,
    /// Number of events received from the remote.
    pub events_received: usize,
    /// Total bytes transferred (approximate, based on serialized event size).
    pub bytes_transferred: usize,
    /// Number of hash-exchange rounds used during the diff phase.
    pub rounds: usize,
}

impl SyncReport {
    /// Returns `true` if the sync was a no-op (replicas already identical).
    pub fn is_noop(&self) -> bool {
        self.events_sent == 0 && self.events_received == 0
    }
}

// ---------------------------------------------------------------------------
// Sync function
// ---------------------------------------------------------------------------

/// Synchronise local events with a remote replica.
///
/// # Protocol
///
/// 1. Build a Prolly Tree from `local_events`.
/// 2. Exchange root hashes with the remote.
/// 3. If hashes match: replicas are identical — return early.
/// 4. Diff the trees to identify event hashes missing from each side.
/// 5. Send local events that the remote is missing.
/// 6. Receive remote events that we are missing.
/// 7. Return a [`SyncReport`] summarising the exchange.
///
/// After sync, the caller is responsible for persisting the received events
/// to the local event log and rebuilding the projection.
pub fn sync<T: SyncTransport>(
    local_events: &[Event],
    transport: &mut T,
) -> Result<(Vec<Event>, SyncReport), T::Error> {
    let local_tree = ProllyTree::build(local_events);
    let mut report = SyncReport {
        events_sent: 0,
        events_received: 0,
        bytes_transferred: 0,
        rounds: 0,
    };

    // Round 1: exchange root hashes.
    transport.send_hash(&local_tree.root.hash())?;
    let remote_root_hash = transport.recv_hash()?;
    report.rounds += 1;

    // Fast path: if root hashes match, replicas are identical.
    if local_tree.root.hash() == remote_root_hash {
        return Ok((vec![], report));
    }

    // Round 2: exchange event hash lists for diff.
    // Send our event hashes so the remote can figure out what we're missing.
    let local_hashes = local_tree.event_hashes();
    transport.send_event_hashes(&local_hashes)?;

    // Receive the remote's event hashes so we know what they have.
    let remote_hashes = transport.recv_event_hashes()?;
    report.rounds += 1;

    // Compute what's missing on each side.
    let local_set: HashSet<&str> = local_hashes.iter().map(|s| s.as_str()).collect();
    let remote_set: HashSet<&str> = remote_hashes.iter().map(|s| s.as_str()).collect();

    // Events we have that the remote doesn't.
    let to_send: Vec<&Event> = local_events
        .iter()
        .filter(|e| !remote_set.contains(e.event_hash.as_str()))
        .collect();

    // Event hashes the remote has that we don't.
    let need_from_remote: HashSet<&str> = remote_hashes
        .iter()
        .map(|s| s.as_str())
        .filter(|h| !local_set.contains(h))
        .collect();

    // Round 3: exchange missing events.
    // Send our events that the remote lacks.
    let events_to_send: Vec<Event> = to_send.into_iter().cloned().collect();
    let send_size: usize = events_to_send
        .iter()
        .map(|e| estimate_event_size(e))
        .sum();
    transport.send_events(&events_to_send)?;
    report.events_sent = events_to_send.len();
    report.bytes_transferred += send_size;

    // Receive events from the remote that we lack.
    let received = transport.recv_events()?;
    let recv_size: usize = received.iter().map(|e| estimate_event_size(e)).sum();
    report.bytes_transferred += recv_size;
    report.rounds += 1;

    // Filter received events to only those we actually need (defence in depth).
    let new_events: Vec<Event> = received
        .into_iter()
        .filter(|e| need_from_remote.contains(e.event_hash.as_str()))
        .collect();
    report.events_received = new_events.len();

    Ok((new_events, report))
}

/// Respond to a sync request as the remote side.
///
/// This is the mirror of [`sync`]: it receives the initiator's data and
/// sends back what they need.
pub fn serve_sync<T: SyncTransport>(
    local_events: &[Event],
    transport: &mut T,
) -> Result<(Vec<Event>, SyncReport), T::Error> {
    let local_tree = ProllyTree::build(local_events);
    let local_hashes = local_tree.event_hashes();
    let mut report = SyncReport {
        events_sent: 0,
        events_received: 0,
        bytes_transferred: 0,
        rounds: 0,
    };

    // Round 1: exchange root hashes (receive first, then send).
    let remote_root_hash = transport.recv_hash()?;
    transport.send_hash(&local_tree.root.hash())?;
    report.rounds += 1;

    // Fast path.
    if local_tree.root.hash() == remote_root_hash {
        return Ok((vec![], report));
    }

    // Round 2: exchange event hash lists.
    let remote_hashes = transport.recv_event_hashes()?;
    transport.send_event_hashes(&local_hashes)?;
    report.rounds += 1;

    // Compute diffs.
    let local_set: HashSet<&str> = local_hashes.iter().map(|s| s.as_str()).collect();
    let remote_set: HashSet<&str> = remote_hashes.iter().map(|s| s.as_str()).collect();

    let need_from_remote: HashSet<&str> = remote_hashes
        .iter()
        .map(|s| s.as_str())
        .filter(|h| !local_set.contains(h))
        .collect();

    let to_send: Vec<Event> = local_events
        .iter()
        .filter(|e| !remote_set.contains(e.event_hash.as_str()))
        .cloned()
        .collect();

    // Round 3: exchange events (receive first, then send).
    let received = transport.recv_events()?;
    let recv_size: usize = received.iter().map(|e| estimate_event_size(e)).sum();
    report.bytes_transferred += recv_size;

    let send_size: usize = to_send.iter().map(|e| estimate_event_size(e)).sum();
    transport.send_events(&to_send)?;
    report.events_sent = to_send.len();
    report.bytes_transferred += send_size;
    report.rounds += 1;

    let new_events: Vec<Event> = received
        .into_iter()
        .filter(|e| need_from_remote.contains(e.event_hash.as_str()))
        .collect();
    report.events_received = new_events.len();

    Ok((new_events, report))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rough estimate of serialized event size (for reporting, not billing).
fn estimate_event_size(event: &Event) -> usize {
    // event_hash + agent + itc + item_id + overhead
    event.event_hash.len()
        + event.agent.len()
        + event.itc.len()
        + event.item_id.as_str().len()
        + 128 // JSON overhead, data payload estimate
}

// ---------------------------------------------------------------------------
// In-memory transport (for testing)
// ---------------------------------------------------------------------------

/// A pair of in-memory channels for testing sync without real I/O.
///
/// Create with [`InMemoryTransport::pair`], which returns two transports
/// connected to each other: what one sends, the other receives.
#[derive(Debug)]
pub struct InMemoryTransport {
    /// Outgoing hash queue.
    tx_hashes: Vec<Hash>,
    /// Incoming hash queue.
    rx_hashes: Vec<Hash>,
    /// Outgoing event-hash-list queue.
    tx_event_hash_lists: Vec<Vec<String>>,
    /// Incoming event-hash-list queue.
    rx_event_hash_lists: Vec<Vec<String>>,
    /// Outgoing event queue.
    tx_events: Vec<Vec<Event>>,
    /// Incoming event queue.
    rx_events: Vec<Vec<Event>>,
}

/// Error type for in-memory transport (should never happen in tests).
#[derive(Debug)]
pub struct InMemoryError(pub String);

impl std::fmt::Display for InMemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InMemoryTransport error: {}", self.0)
    }
}

impl InMemoryTransport {
    /// Create a new empty transport (one side of a pair).
    fn new() -> Self {
        Self {
            tx_hashes: Vec::new(),
            rx_hashes: Vec::new(),
            tx_event_hash_lists: Vec::new(),
            rx_event_hash_lists: Vec::new(),
            tx_events: Vec::new(),
            rx_events: Vec::new(),
        }
    }

    /// Wire two transports together: A's tx → B's rx and vice versa.
    pub fn wire(a: &mut Self, b: &mut Self) {
        // Move A's sent data to B's receive queues.
        b.rx_hashes.extend(a.tx_hashes.drain(..));
        b.rx_event_hash_lists
            .extend(a.tx_event_hash_lists.drain(..));
        b.rx_events.extend(a.tx_events.drain(..));

        // Move B's sent data to A's receive queues.
        a.rx_hashes.extend(b.tx_hashes.drain(..));
        a.rx_event_hash_lists
            .extend(b.tx_event_hash_lists.drain(..));
        a.rx_events.extend(b.tx_events.drain(..));
    }
}

impl SyncTransport for InMemoryTransport {
    type Error = InMemoryError;

    fn send_hash(&mut self, hash: &Hash) -> Result<(), Self::Error> {
        self.tx_hashes.push(*hash);
        Ok(())
    }

    fn recv_hash(&mut self) -> Result<Hash, Self::Error> {
        if self.rx_hashes.is_empty() {
            return Err(InMemoryError("no hash to receive".into()));
        }
        Ok(self.rx_hashes.remove(0))
    }

    fn send_event_hashes(&mut self, hashes: &[String]) -> Result<(), Self::Error> {
        self.tx_event_hash_lists.push(hashes.to_vec());
        Ok(())
    }

    fn recv_event_hashes(&mut self) -> Result<Vec<String>, Self::Error> {
        if self.rx_event_hash_lists.is_empty() {
            return Err(InMemoryError("no event hash list to receive".into()));
        }
        Ok(self.rx_event_hash_lists.remove(0))
    }

    fn send_events(&mut self, events: &[Event]) -> Result<(), Self::Error> {
        self.tx_events.push(events.to_vec());
        Ok(())
    }

    fn recv_events(&mut self) -> Result<Vec<Event>, Self::Error> {
        if self.rx_events.is_empty() {
            return Err(InMemoryError("no events to receive".into()));
        }
        Ok(self.rx_events.remove(0))
    }
}

// ---------------------------------------------------------------------------
// Step-by-step sync helper for InMemoryTransport
// ---------------------------------------------------------------------------

/// Run a full sync between two event sets using in-memory transport.
///
/// Returns the new events each side received and their respective reports.
/// This simulates the 3-round protocol by manually wiring each round.
pub fn sync_in_memory(
    local_events: &[Event],
    remote_events: &[Event],
) -> Result<SyncInMemoryResult, InMemoryError> {
    let mut local_tx = InMemoryTransport::new();
    let mut remote_tx = InMemoryTransport::new();

    // --- Round 1: root hash exchange ---
    let local_tree = ProllyTree::build(local_events);
    let remote_tree = ProllyTree::build(remote_events);

    // Local sends root hash.
    local_tx.send_hash(&local_tree.root.hash())?;
    // Remote sends root hash.
    remote_tx.send_hash(&remote_tree.root.hash())?;
    // Wire round 1.
    InMemoryTransport::wire(&mut local_tx, &mut remote_tx);

    let remote_root = local_tx.recv_hash()?;
    let local_root = remote_tx.recv_hash()?;

    let mut rounds = 1;

    // Fast path: identical.
    if local_tree.root.hash() == remote_root {
        return Ok(SyncInMemoryResult {
            local_received: vec![],
            remote_received: vec![],
            local_report: SyncReport {
                events_sent: 0,
                events_received: 0,
                bytes_transferred: 0,
                rounds,
            },
            remote_report: SyncReport {
                events_sent: 0,
                events_received: 0,
                bytes_transferred: 0,
                rounds,
            },
        });
    }

    // --- Round 2: event hash exchange ---
    let local_hashes = local_tree.event_hashes();
    let remote_hashes = remote_tree.event_hashes();

    local_tx.send_event_hashes(&local_hashes)?;
    remote_tx.send_event_hashes(&remote_hashes)?;
    InMemoryTransport::wire(&mut local_tx, &mut remote_tx);

    let _remote_hash_list = local_tx.recv_event_hashes()?;
    let _local_hash_list = remote_tx.recv_event_hashes()?;
    rounds += 1;

    // Compute diffs.
    let local_set: HashSet<&str> = local_hashes.iter().map(|s| s.as_str()).collect();
    let remote_set: HashSet<&str> = remote_hashes.iter().map(|s| s.as_str()).collect();

    let local_to_send: Vec<Event> = local_events
        .iter()
        .filter(|e| !remote_set.contains(e.event_hash.as_str()))
        .cloned()
        .collect();

    let remote_to_send: Vec<Event> = remote_events
        .iter()
        .filter(|e| !local_set.contains(e.event_hash.as_str()))
        .cloned()
        .collect();

    // --- Round 3: event exchange ---
    let local_send_size: usize = local_to_send.iter().map(|e| estimate_event_size(e)).sum();
    let remote_send_size: usize = remote_to_send
        .iter()
        .map(|e| estimate_event_size(e))
        .sum();

    local_tx.send_events(&local_to_send)?;
    remote_tx.send_events(&remote_to_send)?;
    InMemoryTransport::wire(&mut local_tx, &mut remote_tx);

    let local_received = local_tx.recv_events()?;
    let remote_received = remote_tx.recv_events()?;
    rounds += 1;

    Ok(SyncInMemoryResult {
        local_report: SyncReport {
            events_sent: local_to_send.len(),
            events_received: local_received.len(),
            bytes_transferred: local_send_size + remote_send_size,
            rounds,
        },
        remote_report: SyncReport {
            events_sent: remote_to_send.len(),
            events_received: remote_received.len(),
            bytes_transferred: local_send_size + remote_send_size,
            rounds,
        },
        local_received,
        remote_received,
    })
}

/// Result of an in-memory sync between two replicas.
#[derive(Debug)]
pub struct SyncInMemoryResult {
    /// New events the local side received.
    pub local_received: Vec<Event>,
    /// New events the remote side received.
    pub remote_received: Vec<Event>,
    /// Sync report from the local perspective.
    pub local_report: SyncReport,
    /// Sync report from the remote perspective.
    pub remote_report: SyncReport,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::{CreateData, EventData};
    use crate::event::EventType;
    use crate::model::item::Kind;
    use crate::model::item::Urgency;
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    fn make_event(item_id: &str, ts: i64, hash_suffix: &str) -> Event {
        Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
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
            event_hash: format!("blake3:{item_id}_{ts}_{hash_suffix}"),
        }
    }

    #[test]
    fn sync_identical_replicas_is_noop() {
        let events = vec![
            make_event("a", 1, "x"),
            make_event("b", 2, "y"),
            make_event("c", 3, "z"),
        ];

        let result = sync_in_memory(&events, &events).unwrap();
        assert!(result.local_received.is_empty());
        assert!(result.remote_received.is_empty());
        assert!(result.local_report.is_noop());
        assert!(result.remote_report.is_noop());
        assert_eq!(result.local_report.rounds, 1); // fast path: only root hash round
    }

    #[test]
    fn sync_empty_replicas_is_noop() {
        let result = sync_in_memory(&[], &[]).unwrap();
        assert!(result.local_report.is_noop());
        assert_eq!(result.local_report.rounds, 1);
    }

    #[test]
    fn sync_empty_to_populated() {
        let remote_events = vec![
            make_event("a", 1, "x"),
            make_event("b", 2, "y"),
        ];

        let result = sync_in_memory(&[], &remote_events).unwrap();
        assert_eq!(result.local_received.len(), 2);
        assert!(result.remote_received.is_empty());
        assert_eq!(result.local_report.events_received, 2);
        assert_eq!(result.local_report.events_sent, 0);
    }

    #[test]
    fn sync_populated_to_empty() {
        let local_events = vec![
            make_event("a", 1, "x"),
            make_event("b", 2, "y"),
        ];

        let result = sync_in_memory(&local_events, &[]).unwrap();
        assert!(result.local_received.is_empty());
        assert_eq!(result.remote_received.len(), 2);
        assert_eq!(result.local_report.events_sent, 2);
        assert_eq!(result.local_report.events_received, 0);
    }

    #[test]
    fn sync_diverged_replicas_converge() {
        let shared = vec![make_event("shared", 1, "s")];

        let mut local = shared.clone();
        local.push(make_event("local-only", 2, "l"));

        let mut remote = shared;
        remote.push(make_event("remote-only", 3, "r"));

        let result = sync_in_memory(&local, &remote).unwrap();

        // Local should receive the remote-only event.
        assert_eq!(result.local_received.len(), 1);
        assert_eq!(
            result.local_received[0].event_hash,
            "blake3:remote-only_3_r"
        );

        // Remote should receive the local-only event.
        assert_eq!(result.remote_received.len(), 1);
        assert_eq!(
            result.remote_received[0].event_hash,
            "blake3:local-only_2_l"
        );
    }

    #[test]
    fn sync_is_idempotent() {
        let shared = vec![make_event("s", 1, "s")];
        let mut a = shared.clone();
        a.push(make_event("a-only", 2, "a"));
        let mut b = shared;
        b.push(make_event("b-only", 3, "b"));

        // First sync.
        let r1 = sync_in_memory(&a, &b).unwrap();

        // After sync, both sides have all events.
        let mut a_merged = a.clone();
        a_merged.extend(r1.local_received);
        let mut b_merged = b.clone();
        b_merged.extend(r1.remote_received);

        // Second sync — should be a no-op.
        let r2 = sync_in_memory(&a_merged, &b_merged).unwrap();
        assert!(r2.local_report.is_noop());
        assert!(r2.remote_report.is_noop());
        assert_eq!(r2.local_report.rounds, 1); // fast path
    }

    #[test]
    fn sync_concurrent_same_item() {
        // Both sides created events for the same item at different times.
        let a_events = vec![
            make_event("item-1", 100, "agent-a"),
            make_event("item-1", 200, "agent-a-update"),
        ];
        let b_events = vec![
            make_event("item-1", 150, "agent-b"),
            make_event("item-1", 250, "agent-b-update"),
        ];

        let result = sync_in_memory(&a_events, &b_events).unwrap();

        // Each side should receive the other's events.
        assert_eq!(result.local_received.len(), 2);
        assert_eq!(result.remote_received.len(), 2);
    }

    #[test]
    fn sync_large_divergence() {
        // 100 shared events, 50 unique on each side.
        let shared: Vec<Event> = (0..100)
            .map(|i| make_event(&format!("s{i:03}"), i, &format!("s{i}")))
            .collect();

        let mut a = shared.clone();
        for i in 0..50 {
            a.push(make_event(
                &format!("a{i:03}"),
                1000 + i,
                &format!("a{i}"),
            ));
        }

        let mut b = shared;
        for i in 0..50 {
            b.push(make_event(
                &format!("b{i:03}"),
                2000 + i,
                &format!("b{i}"),
            ));
        }

        let result = sync_in_memory(&a, &b).unwrap();
        assert_eq!(result.local_received.len(), 50);
        assert_eq!(result.remote_received.len(), 50);
        assert_eq!(result.local_report.rounds, 3);
    }

    #[test]
    fn sync_report_bytes_nonzero() {
        let a = vec![make_event("a", 1, "x")];
        let b = vec![make_event("b", 2, "y")];

        let result = sync_in_memory(&a, &b).unwrap();
        assert!(result.local_report.bytes_transferred > 0);
    }

    #[test]
    fn sync_report_is_noop() {
        let report = SyncReport {
            events_sent: 0,
            events_received: 0,
            bytes_transferred: 0,
            rounds: 1,
        };
        assert!(report.is_noop());

        let report2 = SyncReport {
            events_sent: 1,
            events_received: 0,
            bytes_transferred: 100,
            rounds: 3,
        };
        assert!(!report2.is_noop());
    }

    #[test]
    fn sync_many_small_events() {
        // Stress test: 500 events on each side with minimal overlap.
        let a: Vec<Event> = (0..500)
            .map(|i| make_event(&format!("a{i:04}"), i, &format!("a{i}")))
            .collect();
        let b: Vec<Event> = (0..500)
            .map(|i| make_event(&format!("b{i:04}"), i, &format!("b{i}")))
            .collect();

        let result = sync_in_memory(&a, &b).unwrap();
        assert_eq!(result.local_received.len(), 500);
        assert_eq!(result.remote_received.len(), 500);
    }

    #[test]
    fn sync_one_side_subset_of_other() {
        // Local has events 0..10, remote has 0..20.
        // Local should receive events 10..20.
        let all: Vec<Event> = (0..20)
            .map(|i| make_event(&format!("e{i:03}"), i, &format!("h{i}")))
            .collect();

        let local = &all[0..10];
        let remote = &all[..];

        let result = sync_in_memory(local, remote).unwrap();
        assert_eq!(result.local_received.len(), 10);
        assert!(result.remote_received.is_empty());
    }

    #[test]
    fn estimate_size_is_reasonable() {
        let e = make_event("test-item", 12345, "abc");
        let size = estimate_event_size(&e);
        assert!(size > 50, "Event size estimate too small: {size}");
        assert!(size < 500, "Event size estimate too large: {size}");
    }
}
