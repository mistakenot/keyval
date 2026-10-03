//! Bounded collections (PLAN.md §7.11, CD-9): every queue and map in the core has a
//! named limit from the registry ([`crate::Limit`]), and hitting it is a typed outcome
//! that hands the refused item back, never silent growth.

use std::collections::{BTreeMap, VecDeque};

/// Which bound a [`BoundedQueue`] push would have exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueFull {
    /// The queue already holds `limit` items.
    Items {
        /// The item bound.
        limit: usize,
    },
    /// The item's bytes would take the queue past `limit` bytes.
    Bytes {
        /// The byte bound.
        limit: u64,
        /// The queued bytes the push would have produced.
        would_be: u64,
    },
}

/// A refused push: the item, returned unchanged, and the bound it hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueRefused<T> {
    /// The item that was not queued.
    pub item: T,
    /// The bound it would have exceeded.
    pub full: QueueFull,
}

/// A FIFO queue bounded by an item count and a total byte weight (§7.11: pending
/// attempts, per-peer send queues, the controller queue, the log queue).
///
/// Invariants: at most `max_items` items, whose weights sum to `bytes`, at most
/// `max_bytes`. A refused push changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedQueue<T> {
    /// The items in arrival order, each with its byte weight.
    items: VecDeque<(T, u64)>,
    /// The item bound.
    max_items: usize,
    /// The byte bound.
    max_bytes: u64,
    /// The sum of the queued weights.
    bytes: u64,
}

impl<T> BoundedQueue<T> {
    /// An empty queue holding at most `max_items` items and `max_bytes` bytes.
    #[must_use]
    pub const fn new(max_items: usize, max_bytes: u64) -> Self {
        Self {
            items: VecDeque::new(),
            max_items,
            max_bytes,
            bytes: 0,
        }
    }

    /// An empty queue bounded by items only (the controller and log queues).
    #[must_use]
    pub const fn with_items(max_items: usize) -> Self {
        Self::new(max_items, u64::MAX)
    }

    /// Appends `item`, whose byte weight is `bytes`.
    ///
    /// # Errors
    ///
    /// [`QueueRefused`], carrying the item back, if the queue already holds `max_items` items
    /// or the push would take it past `max_bytes` bytes. The queue is unchanged.
    pub fn push(&mut self, item: T, bytes: u64) -> Result<(), QueueRefused<T>> {
        if self.items.len() >= self.max_items {
            return Err(QueueRefused {
                item,
                full: QueueFull::Items {
                    limit: self.max_items,
                },
            });
        }
        let would_be = self.bytes.saturating_add(bytes);
        if would_be > self.max_bytes {
            return Err(QueueRefused {
                item,
                full: QueueFull::Bytes {
                    limit: self.max_bytes,
                    would_be,
                },
            });
        }
        self.items.push_back((item, bytes));
        self.bytes = would_be;
        Ok(())
    }

    /// Removes and returns the oldest item.
    pub fn pop(&mut self) -> Option<T> {
        let (item, bytes) = self.items.pop_front()?;
        self.bytes = self.bytes.saturating_sub(bytes);
        Some(item)
    }

    /// The oldest item, if any.
    #[must_use]
    pub fn front(&self) -> Option<&T> {
        self.items.front().map(|(item, _)| item)
    }

    /// The number of queued items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The queued bytes.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The item bound.
    #[must_use]
    pub const fn max_items(&self) -> usize {
        self.max_items
    }

    /// The byte bound.
    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The queued items, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter().map(|(item, _)| item)
    }
}

/// A refused insert: the entry, returned unchanged, and the bound it hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapFull<K, V> {
    /// The key that was not inserted.
    pub key: K,
    /// Its value.
    pub value: V,
    /// The entry bound.
    pub limit: usize,
}

/// An ordered map bounded by its number of entries (§7.11, e.g. waiters per request
/// ID). It never hashes and iterates in key order (§7.14).
///
/// Invariant: at most `max_entries` entries. A refused insert changes nothing; replacing
/// an existing key's value is never refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedMap<K, V> {
    /// The entries.
    entries: BTreeMap<K, V>,
    /// The entry bound.
    max_entries: usize,
}

impl<K: Ord, V> BoundedMap<K, V> {
    /// An empty map holding at most `max_entries` entries.
    #[must_use]
    pub const fn new(max_entries: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_entries,
        }
    }

    /// Inserts `value` at `key`, returning the value it replaced, if any.
    ///
    /// # Errors
    ///
    /// [`MapFull`], carrying the entry back, if `key` is new and the map already holds
    /// `max_entries` entries. The map is unchanged.
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<V>, MapFull<K, V>> {
        if !self.entries.contains_key(&key) && self.entries.len() >= self.max_entries {
            return Err(MapFull {
                key,
                value,
                limit: self.max_entries,
            });
        }
        Ok(self.entries.insert(key, value))
    }

    /// Removes `key`, returning its value, if any.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.entries.remove(key)
    }

    /// The value at `key`, if any.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    /// The value at `key`, mutably, if any.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.entries.get_mut(key)
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry bound.
    #[must_use]
    pub const fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// The entries, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::path::Path;

    use super::{BoundedMap, BoundedQueue, MapFull, QueueFull, QueueRefused};
    use crate::{Config, Limit};

    /// A seeded `SplitMix64` step: the property tests' only randomness (§7.14).
    fn next(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    #[test]
    fn a_queue_accepts_n_minus_1_and_n_items_and_refuses_n_plus_1() {
        for n in [1_usize, 2, 3, 16] {
            let mut queue = BoundedQueue::with_items(n);
            for item in 0..n {
                assert_eq!(
                    queue.push(item, 1),
                    Ok(()),
                    "rule: item {} of {n} fits",
                    item + 1
                );
            }
            assert_eq!(
                queue.push(n, 1),
                Err(QueueRefused {
                    item: n,
                    full: QueueFull::Items { limit: n }
                }),
                "rule: item N+1 is refused and handed back (CD-9); n={n}"
            );
            assert_eq!(queue.len(), n, "rule: a refused push changes nothing");
            assert_eq!(queue.pop(), Some(0), "rule: FIFO");
            assert_eq!(queue.push(n, 1), Ok(()), "rule: room again after a pop");
        }
    }

    #[test]
    fn a_queue_accepts_n_minus_1_and_n_bytes_and_refuses_n_plus_1() {
        for n in [1_u64, 2, 100, 8 << 20] {
            let mut below = BoundedQueue::new(10, n);
            assert_eq!(below.push("a", n - 1), Ok(()), "rule: N-1 bytes fit; n={n}");
            let mut exact = BoundedQueue::new(10, n);
            assert_eq!(exact.push("a", n - 1), Ok(()));
            assert_eq!(
                exact.push("b", 1),
                Ok(()),
                "rule: exactly N bytes fit; n={n}"
            );
            assert_eq!(exact.bytes(), n);
            assert_eq!(
                exact.push("c", 1),
                Err(QueueRefused {
                    item: "c",
                    full: QueueFull::Bytes {
                        limit: n,
                        would_be: n + 1
                    }
                }),
                "rule: N+1 bytes are refused (CD-9); n={n}"
            );
            assert_eq!(exact.pop(), Some("a"));
            assert_eq!(exact.bytes(), 1, "rule: popping releases the item's bytes");
        }
    }

    #[test]
    fn the_registry_s_queue_limits_hold_at_n_minus_1_n_and_n_plus_1() {
        let config = Config::parse(
            include_str!("../../../config/local.toml"),
            Path::new("config"),
        )
        .unwrap();
        let item_bounds = [
            Limit::PendingAttempts,
            Limit::WaitersPerRequest,
            Limit::PeerQueueMessages,
            Limit::ControllerQueueInputs,
            Limit::LogQueueRecords,
        ];
        for limit in item_bounds {
            let n = usize::try_from(config.limit(limit)).unwrap();
            let mut queue = BoundedQueue::with_items(n);
            for item in 0..n - 1 {
                queue.push(item, 0).unwrap();
            }
            assert_eq!(
                queue.push(n - 1, 0),
                Ok(()),
                "rule: the N-th fits; {}",
                limit.name()
            );
            assert_eq!(
                queue.push(n, 0).map_err(|refused| refused.full),
                Err(QueueFull::Items { limit: n }),
                "rule: the (N+1)-th is refused; {}",
                limit.name()
            );
        }
        for (items, bytes) in [
            (Limit::PendingAttempts, Limit::PendingAttemptBytes),
            (Limit::PeerQueueMessages, Limit::PeerQueueBytes),
        ] {
            let n = config.limit(bytes);
            let mut queue = BoundedQueue::new(usize::try_from(config.limit(items)).unwrap(), n);
            assert_eq!(
                queue.push((), n - 1),
                Ok(()),
                "rule: N-1 bytes; {}",
                bytes.name()
            );
            assert_eq!(queue.push((), 1), Ok(()), "rule: N bytes; {}", bytes.name());
            assert_eq!(
                queue.push((), 1).map_err(|refused| refused.full),
                Err(QueueFull::Bytes {
                    limit: n,
                    would_be: n + 1
                }),
                "rule: N+1 bytes are refused; {}",
                bytes.name()
            );
        }
    }

    #[test]
    fn a_map_accepts_n_minus_1_and_n_entries_refuses_n_plus_1_and_always_replaces() {
        for n in [1_usize, 2, 16] {
            let mut map = BoundedMap::new(n);
            for key in 0..n {
                assert_eq!(
                    map.insert(key, "v"),
                    Ok(None),
                    "rule: entry {} of {n} fits",
                    key + 1
                );
            }
            assert_eq!(
                map.insert(n, "new"),
                Err(MapFull {
                    key: n,
                    value: "new",
                    limit: n
                }),
                "rule: a new key past N is refused and handed back (CD-9); n={n}"
            );
            assert_eq!(
                map.insert(0, "replaced"),
                Ok(Some("v")),
                "rule: replacing never grows"
            );
            assert_eq!(map.len(), n);
            assert_eq!(map.remove(&0), Some("replaced"));
            assert_eq!(
                map.insert(n, "new"),
                Ok(None),
                "rule: room again after a remove"
            );
        }
    }

    #[test]
    fn random_queue_operations_never_exceed_either_bound_and_keep_fifo_order() {
        for seed in 0..64_u64 {
            let mut state = seed;
            let max_items = usize::try_from(next(&mut state) % 8).unwrap() + 1;
            let max_bytes = next(&mut state) % 64 + 1;
            let mut queue = BoundedQueue::new(max_items, max_bytes);
            let mut model: VecDeque<(u64, u64)> = VecDeque::new();
            for step in 0..200_u64 {
                if next(&mut state).is_multiple_of(3) {
                    assert_eq!(queue.pop(), model.pop_front().map(|(item, _)| item));
                } else {
                    let bytes = next(&mut state) % 16;
                    let fits = model.len() < max_items
                        && model.iter().map(|(_, b)| b).sum::<u64>() + bytes <= max_bytes;
                    match queue.push(step, bytes) {
                        Ok(()) => {
                            assert!(fits, "rule: only a fitting push is accepted; seed={seed}");
                            model.push_back((step, bytes));
                        }
                        Err(refused) => {
                            assert!(!fits, "rule: a fitting push is never refused; seed={seed}");
                            assert_eq!(refused.item, step, "rule: the item is handed back");
                        }
                    }
                }
                assert!(
                    queue.len() <= max_items && queue.bytes() <= max_bytes,
                    "rule: neither bound is ever exceeded (CD-9); seed={seed} step={step}"
                );
                assert_eq!(
                    queue.iter().copied().collect::<Vec<_>>(),
                    model.iter().map(|(item, _)| *item).collect::<Vec<_>>(),
                    "rule: FIFO order; seed={seed} step={step}"
                );
            }
        }
    }

    #[test]
    fn random_map_operations_never_exceed_the_bound() {
        for seed in 0..64_u64 {
            let mut state = seed;
            let max_entries = usize::try_from(next(&mut state) % 8).unwrap() + 1;
            let mut map = BoundedMap::new(max_entries);
            let mut model: BTreeMap<u64, u64> = BTreeMap::new();
            for step in 0..200_u64 {
                let key = next(&mut state) % 12;
                if next(&mut state).is_multiple_of(3) {
                    assert_eq!(map.remove(&key), model.remove(&key));
                } else {
                    let fits = model.contains_key(&key) || model.len() < max_entries;
                    let result = map.insert(key, step);
                    assert_eq!(
                        result.is_ok(),
                        fits,
                        "rule: refused exactly when full; seed={seed}"
                    );
                    if fits {
                        model.insert(key, step);
                    }
                }
                assert!(
                    map.len() <= max_entries,
                    "rule: the bound holds; seed={seed} step={step}"
                );
                assert_eq!(
                    map.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
                    model.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
                    "rule: the map agrees with its model, in key order; seed={seed}"
                );
            }
        }
    }
}
