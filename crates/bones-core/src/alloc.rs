use bumpalo::Bump;
use crate::event::Event;

/// Arena for allocating Events during replay.
/// All allocations are freed at once when the arena is dropped.
pub struct EventArena {
    bump: Bump,
}

impl Default for EventArena {
    fn default() -> Self {
        Self::new()
    }
}

impl EventArena {
    /// Create a new arena.
    pub fn new() -> Self {
        Self {
            bump: Bump::new(),
        }
    }

    /// Create a new arena with the given initial capacity in bytes.
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            bump: Bump::with_capacity(bytes),
        }
    }

    /// Allocate an Event in the arena. Returns a reference that lives
    /// as long as the arena.
    pub fn alloc_event<'a>(&'a self, event: Event) -> &'a Event {
        self.bump.alloc(event)
    }

    /// Allocate a slice of Events in the arena.
    pub fn alloc_slice<'a>(&'a self, events: &[Event]) -> &'a [Event] {
        self.bump.alloc_slice_clone(events)
    }

    /// Reset the arena for reuse (frees all allocations without deallocating
    /// the underlying memory).
    pub fn reset(&mut self) {
        self.bump.reset();
    }
}
