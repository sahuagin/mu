//! A bounded, insertion-ordered set of recently-recorded keys.
//!
//! Several independent parts of the gateway remember "have I already done this
//! one?" about a stream driven by remote senders: the mesh→IRC router's
//! exactly-once endpoint/observer overlap key, its per-human absent-notice
//! suppression, and the IRC→mesh side's set of ids the gateway itself minted
//! (the loop guard). All are answering the same shape of question about the
//! same kind of traffic, so all use this one policy rather than growing
//! divergent ones — and, crucially, none of them may grow with total traffic
//! from senders the gateway does not choose.
//!
//! The policy is a fixed-capacity window of the most recently *recorded* keys.
//! When the window is full, the oldest key is evicted to make room. It is NOT
//! an LRU: re-observing a key does not refresh its position, because the thing
//! being bounded is how far back in the stream a duplicate can arrive, not how
//! popular a key is. A key can also be dropped explicitly ([`RecentSet::remove`])
//! when something the gateway observed makes it no longer interesting.
//!
//! The window bounds the NUMBER of keys, which is only half a memory bound: a
//! key is stored twice (once in the order queue, once in the set), so the other
//! half is the caller's — a key whose SIZE a remote sender chooses must be
//! reduced to a fixed-size form before it is recorded here. Each user does that
//! at its own boundary: the exactly-once key is a 32-byte digest of its
//! `(id, destination, session)` tuple, the absent-notice key is a folded nick
//! the framing rules already bound, and the loop guard holds ids the gateway
//! itself minted. This type deliberately does not guess at a byte budget for
//! them; it just never grows past `capacity` entries.
//!
//! The window is what makes the memory bounded, and it is also the honest limit
//! of the guarantee. Both users care about a duplicate that follows its
//! original by a few messages — an endpoint/observer overlap of one minted id,
//! or a publish observed back on the wildcard a moment later — so
//! [`DEFAULT_CAPACITY`] is set far above that gap. A duplicate separated from
//! its original by more than a full window is treated as new; an unbounded set
//! would catch that case, at the cost of growing with total traffic for the
//! life of the connection.

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// How many keys a gateway window keeps.
///
/// Both users need to span only the small gap between a message and its
/// duplicate: the endpoint/observer overlap delivers the same id twice within
/// one dispatch, and a self-published id comes back on the observer wildcard on
/// the order of a network round trip. 4096 is several orders of magnitude more
/// than either needs, and — with the fixed-size keys the module note above asks
/// callers for — costs a few hundred kilobytes at worst.
pub const DEFAULT_CAPACITY: usize = 4096;

/// A bounded set of recently recorded keys: membership answers "recorded
/// recently", and the oldest entry is evicted once `capacity` is reached.
#[derive(Debug, Clone)]
pub struct RecentSet<T: Eq + Hash + Clone> {
    capacity: usize,
    /// Recorded keys in insertion order; the front is the oldest.
    order: VecDeque<T>,
    /// The same keys, for O(1) membership.
    set: HashSet<T>,
}

impl<T: Eq + Hash + Clone> RecentSet<T> {
    /// A window holding at most `capacity` keys. A zero capacity is treated as
    /// one, so `record` still answers correctly rather than dividing by zero at
    /// the edge.
    pub fn with_capacity(capacity: usize) -> Self {
        RecentSet {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    /// Record `key`, returning `true` when it was NOT already in the window —
    /// i.e. `true` means "new, act on it" and `false` means "a duplicate".
    ///
    /// A duplicate does not move the key's position: its eviction is still
    /// timed from when it was first recorded.
    pub fn record(&mut self, key: T) -> bool {
        if self.set.contains(&key) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.set.insert(key);
        true
    }

    /// Whether `key` is currently in the window.
    pub fn contains(&self, key: &T) -> bool {
        self.set.contains(key)
    }

    /// Forget one key, returning whether it was in the window.
    ///
    /// The remaining keys keep their relative order, so eviction still runs
    /// oldest-first afterwards — removal takes a key OUT of the window, it does
    /// not reorder the ones around it. This exists because one of the window's
    /// users has an explicit "this key is no longer interesting" event (a human
    /// coming back clears their absent-notice suppression) rather than only
    /// aging out.
    pub fn remove(&mut self, key: &T) -> bool {
        if !self.set.remove(key) {
            return false;
        }
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        true
    }

    /// How many keys the window currently holds.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether the window is empty.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The window's fixed capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Forget everything (a reconnect: the disposable state goes with it).
    pub fn clear(&mut self) {
        self.order.clear();
        self.set.clear();
    }
}

impl<T: Eq + Hash + Clone> Default for RecentSet<T> {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}
