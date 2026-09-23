/// A pool-local frame index, not a stored-page address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferId(u32);

impl BufferId {
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Replacement policy independent of page storage and pin accounting.
///
/// Callbacks run under the pool's metadata mutex and must not reenter the pool.
/// `access` records a client cache-hit pin; `load` records successful publication
/// of a new page in a buffer. Internal flush pins generate neither event.
/// `victim` must return an eligible
/// buffer, or `None` only if it found no eligible buffer. The pool always prefers
/// free frames without consulting `victim`, and validates every returned index
/// and pin count: a faulty policy can impair availability, but not pin safety.
pub trait VictimStrategy: Send {
    fn access(&mut self, buffer: BufferId);
    /// Reset per-page history when a slot is assigned to a new page. Policies
    /// such as FIFO or LRU-K can distinguish installation from a cache hit.
    fn load(&mut self, buffer: BufferId) {
        self.access(buffer);
    }
    /// Selection does not confirm removal: subsequent I/O may fail. Keep the
    /// candidate tracked for future selection until load confirms reassignment.
    fn victim(&mut self, can_evict: &dyn Fn(BufferId) -> bool) -> Option<BufferId>;
}

/// Second-chance replacement with policy-owned reference bits and clock hand.
pub struct ClockStrategy {
    referenced: Box<[bool]>,
    hand: usize,
}

impl ClockStrategy {
    /// Construct a clock for `capacity` frames. An empty clock has no victim.
    /// Panics if the capacity exceeds the BufferId address space.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity == 0 || u32::try_from(capacity - 1).is_ok());
        Self {
            referenced: vec![false; capacity].into_boxed_slice(),
            hand: 0,
        }
    }
}

impl VictimStrategy for ClockStrategy {
    fn access(&mut self, buffer: BufferId) {
        self.referenced[buffer.get() as usize] = true;
    }

    fn victim(&mut self, can_evict: &dyn Fn(BufferId) -> bool) -> Option<BufferId> {
        // First sweep clears eligible reference bits; the second selects one.
        for _ in 0..2 {
            for _ in 0..self.referenced.len() {
                let index = self.hand;
                self.hand = (self.hand + 1) % self.referenced.len();
                let buffer = BufferId::new(index as u32);
                if !can_evict(buffer) {
                    continue;
                }
                if !self.referenced[index] {
                    return Some(buffer);
                }
                self.referenced[index] = false;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_id_covers_u32() {
        const ZERO: BufferId = BufferId::new(0);
        const MAX: BufferId = BufferId::new(u32::MAX);
        assert_eq!(ZERO.get(), 0);
        assert_eq!(MAX.get(), u32::MAX);
        assert_eq!(size_of::<BufferId>(), size_of::<u32>());
    }

    #[test]
    fn clock_second_chance_and_eligibility() {
        let mut clock = ClockStrategy::new(3);
        for index in 0..3 {
            clock.access(BufferId::new(index));
        }
        assert_eq!(clock.victim(&|_| false), None);
        assert_eq!(clock.victim(&|id| id.get() != 0), Some(BufferId::new(1)));
        clock.access(BufferId::new(2));
        assert_eq!(clock.victim(&|_| true), Some(BufferId::new(1)));
        assert_eq!(clock.victim(&|_| true), Some(BufferId::new(2)));
    }

    #[test]
    fn empty_clock_never_consults_eligibility() {
        assert_eq!(
            ClockStrategy::new(0).victim(&|_| panic!("empty clock")),
            None
        );
    }
}
