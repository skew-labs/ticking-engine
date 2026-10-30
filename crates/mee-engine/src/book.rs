//! Price-level order book: a `BTreeMap` of levels, each a FIFO linked list
//! of orders in a slab. Insert is O(log levels), cancel by id is O(1) plus a
//! level removal when it empties, and the best price is O(log levels).
//!
//! The book's commitment (an additive multiset hash of its orders) is not
//! updated on every change: the first change to an order in a block records
//! its prior state, and `flush_digest` (at commit) hashes only orders whose
//! state differs from the start of the block. An order placed and cancelled
//! within a block costs no hashing, and because the hash is additive the
//! flush can be split across cores in any order.

use std::collections::BTreeMap;

use mee_core::Side;
use rustc_hash::FxHashMap;

use crate::state::{AccountId, OrderId};

const NIL: u32 = u32::MAX;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    pub account: AccountId,
    pub side: Side,
    pub price: u64,
    pub remaining: u64,
    pub client_id: u64,
}

#[derive(Clone, Debug)]
struct Node {
    order: Order,
    prev: u32,
    next: u32,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Level {
    head: u32,
    tail: u32,
    pub total: u64,
    pub count: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Book {
    bids: BTreeMap<u64, Level>,
    asks: BTreeMap<u64, Level>,
    slab: Vec<Node>,
    free: Vec<u32>,
    /// Lookup only; never iterated, so state transitions stay deterministic.
    index: FxHashMap<OrderId, u32>,
    /// Multiset hash of the resting orders as of the last flush.
    digest: crate::commit::MultisetHash,
    /// Orders changed since the last flush, with their state before the first
    /// change (`None` = didn't exist). Lookup and summing only, so iteration
    /// order never matters.
    touched: FxHashMap<OrderId, Option<Order>>,
}

/// Orders flushed per thread before splitting the work pays off.
const PARALLEL_FLUSH: usize = 2_048;

impl Book {
    fn levels(&self, side: Side) -> &BTreeMap<u64, Level> {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    fn levels_mut(&mut self, side: Side) -> &mut BTreeMap<u64, Level> {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn best_price(&self, side: Side) -> Option<u64> {
        match side {
            Side::Bid => self.bids.last_key_value().map(|(p, _)| *p),
            Side::Ask => self.asks.first_key_value().map(|(p, _)| *p),
        }
    }

    /// Commitment to the current orders (hashes pending changes without
    /// recording them; `flush_digest` does both).
    pub fn digest(&self) -> crate::commit::MultisetHash {
        let mut d = self.digest;
        d.merge(&self.pending_delta());
        d
    }

    /// Folds pending changes into the stored digest (called at commit).
    pub fn flush_digest(&mut self) {
        if self.touched.is_empty() {
            return;
        }
        let delta = self.pending_delta();
        self.digest.merge(&delta);
        self.touched.clear();
    }

    /// Sum of `+hash(now) − hash(before)` over touched orders, split across
    /// threads when there are many (the sum is order-independent).
    fn pending_delta(&self) -> crate::commit::Delta {
        use crate::commit::{Delta, order_hash};
        let one = |(id, before): (&OrderId, &Option<Order>), d: &mut Delta| {
            let now = self.get(*id);
            if now == before.as_ref() {
                return;
            }
            if let Some(b) = before {
                d.sub(&order_hash(b));
            }
            if let Some(n) = now {
                d.add(&order_hash(n));
            }
        };
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        if threads == 1 || self.touched.len() < 2 * PARALLEL_FLUSH {
            let mut d = Delta::default();
            self.touched.iter().for_each(|e| one(e, &mut d));
            return d;
        }
        let entries: Vec<(&OrderId, &Option<Order>)> = self.touched.iter().collect();
        let chunk = entries.len().div_ceil(threads).max(PARALLEL_FLUSH);
        std::thread::scope(|scope| {
            let parts: Vec<_> = entries
                .chunks(chunk)
                .map(|c| {
                    scope.spawn(move || {
                        let mut d = Delta::default();
                        c.iter().for_each(|e| one(*e, &mut d));
                        d
                    })
                })
                .collect();
            let mut total = Delta::default();
            for p in parts {
                total.merge(&p.join().expect("flush worker"));
            }
            total
        })
    }

    /// Remembers an order's state before its first change in this block.
    fn touch(&mut self, id: OrderId) {
        if !self.touched.contains_key(&id) {
            let before = self.get(id).copied();
            self.touched.insert(id, before);
        }
    }

    pub fn get(&self, id: OrderId) -> Option<&Order> {
        self.index.get(&id).map(|&i| &self.slab[i as usize].order)
    }

    /// Appends to the back of its price level.
    pub fn insert(&mut self, order: Order) {
        debug_assert!(!self.index.contains_key(&order.id));
        self.touch(order.id);
        let node = Node { order, prev: NIL, next: NIL };
        let idx = match self.free.pop() {
            Some(i) => {
                self.slab[i as usize] = node;
                i
            }
            None => {
                self.slab.push(node);
                (self.slab.len() - 1) as u32
            }
        };
        self.index.insert(order.id, idx);
        let level = self.levels_mut(order.side).entry(order.price).or_insert(Level {
            head: NIL,
            tail: NIL,
            total: 0,
            count: 0,
        });
        let tail = level.tail;
        level.tail = idx;
        if tail == NIL {
            level.head = idx;
        }
        level.total += order.remaining;
        level.count += 1;
        if tail != NIL {
            self.slab[tail as usize].next = idx;
            self.slab[idx as usize].prev = tail;
        }
    }

    /// The order queued right behind `id` at its price, if any.
    pub fn next_in_level(&self, id: OrderId) -> Option<OrderId> {
        let n = &self.slab[*self.index.get(&id)? as usize];
        (n.next != NIL).then(|| self.slab[n.next as usize].order.id)
    }

    /// Puts `order` back in front of `before` (same side and price), or at
    /// the back of its level if `before` is `None`: undoes a `remove`
    /// without losing time priority.
    pub fn insert_before(&mut self, order: Order, before: Option<OrderId>) {
        let Some(&b) = before.and_then(|id| self.index.get(&id)) else {
            return self.insert(order);
        };
        debug_assert!(!self.index.contains_key(&order.id));
        self.touch(order.id);
        let prev = self.slab[b as usize].prev;
        let node = Node { order, prev, next: b };
        let idx = match self.free.pop() {
            Some(i) => {
                self.slab[i as usize] = node;
                i
            }
            None => {
                self.slab.push(node);
                (self.slab.len() - 1) as u32
            }
        };
        self.index.insert(order.id, idx);
        self.slab[b as usize].prev = idx;
        if prev != NIL {
            self.slab[prev as usize].next = idx;
        }
        let level = self.levels_mut(order.side).get_mut(&order.price).expect("level of the order behind");
        if level.head == b {
            level.head = idx;
        }
        level.total += order.remaining;
        level.count += 1;
    }

    pub fn remove(&mut self, id: OrderId) -> Option<Order> {
        self.touch(id);
        let idx = self.index.remove(&id)?;
        let Node { order, prev, next } = self.slab[idx as usize].clone();
        if prev != NIL {
            self.slab[prev as usize].next = next;
        }
        if next != NIL {
            self.slab[next as usize].prev = prev;
        }
        let levels = self.levels_mut(order.side);
        let level = levels.get_mut(&order.price).expect("level of live order");
        if level.head == idx {
            level.head = next;
        }
        if level.tail == idx {
            level.tail = prev;
        }
        level.total -= order.remaining;
        level.count -= 1;
        if level.count == 0 {
            levels.remove(&order.price);
        }
        self.free.push(idx);
        Some(order)
    }

    /// Shrinks a resting order after a partial fill.
    pub fn reduce(&mut self, id: OrderId, qty: u64) {
        self.touch(id);
        let idx = self.index[&id];
        let order = &mut self.slab[idx as usize].order;
        order.remaining -= qty;
        let (side, price) = (order.side, order.price);
        self.levels_mut(side).get_mut(&price).expect("level of live order").total -= qty;
    }

    /// Orders on `side` in priority order (best price first, then time).
    /// A concrete iterator: no allocation or dynamic dispatch on the
    /// matching path.
    pub fn iter(&self, side: Side) -> OrderIter<'_> {
        let levels = match side {
            Side::Bid => Levels::Down(self.bids.values().rev()),
            Side::Ask => Levels::Up(self.asks.values()),
        };
        OrderIter { slab: &self.slab, levels, cur: NIL }
    }

    /// Aggregated depth: up to `n` levels of (price, total size).
    pub fn depth(&self, side: Side, n: usize) -> Vec<(u64, u64)> {
        let it: Box<dyn Iterator<Item = (&u64, &Level)>> = match side {
            Side::Bid => Box::new(self.levels(side).iter().rev()),
            Side::Ask => Box::new(self.levels(side).iter()),
        };
        it.take(n).map(|(p, l)| (*p, l.total)).collect()
    }

    /// Removes every order, returning them in priority order per side.
    pub fn clear(&mut self) -> Vec<Order> {
        let mut out: Vec<Order> = self.iter(Side::Bid).copied().collect();
        out.extend(self.iter(Side::Ask).copied());
        *self = Book::default();
        out
    }
}

enum Levels<'a> {
    Up(std::collections::btree_map::Values<'a, u64, Level>),
    Down(std::iter::Rev<std::collections::btree_map::Values<'a, u64, Level>>),
}

/// See `Book::iter`.
pub struct OrderIter<'a> {
    slab: &'a [Node],
    levels: Levels<'a>,
    cur: u32,
}

impl<'a> Iterator for OrderIter<'a> {
    type Item = &'a Order;

    #[inline]
    fn next(&mut self) -> Option<&'a Order> {
        while self.cur == NIL {
            let level = match &mut self.levels {
                Levels::Up(it) => it.next(),
                Levels::Down(it) => it.next(),
            }?;
            self.cur = level.head;
        }
        let n = &self.slab[self.cur as usize];
        self.cur = n.next;
        Some(&n.order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(id: u64, side: Side, price: u64, qty: u64) -> Order {
        Order { id, account: 0, side, price, remaining: qty, client_id: 0 }
    }

    #[test]
    fn priority_and_cancel() {
        let mut b = Book::default();
        b.insert(o(1, Side::Bid, 100, 5));
        b.insert(o(2, Side::Bid, 101, 5));
        b.insert(o(3, Side::Bid, 100, 7));
        b.insert(o(4, Side::Ask, 105, 1));
        let ids: Vec<u64> = b.iter(Side::Bid).map(|o| o.id).collect();
        assert_eq!(ids, vec![2, 1, 3]);
        assert_eq!(b.best_price(Side::Bid), Some(101));
        assert_eq!(b.best_price(Side::Ask), Some(105));
        assert_eq!(b.depth(Side::Bid, 5), vec![(101, 5), (100, 12)]);

        b.remove(1);
        assert_eq!(b.depth(Side::Bid, 5), vec![(101, 5), (100, 7)]);
        b.remove(2);
        assert_eq!(b.best_price(Side::Bid), Some(100));
        b.reduce(3, 2);
        assert_eq!(b.get(3).unwrap().remaining, 5);
        assert_eq!(b.depth(Side::Bid, 5), vec![(100, 5)]);

        // The lazily maintained digest equals hashing the orders from scratch,
        // flushed or not.
        let fresh = |b: &Book| {
            let mut d = crate::commit::MultisetHash::default();
            for side in [Side::Bid, Side::Ask] {
                b.iter(side).for_each(|o| d.add(&crate::commit::order_hash(o)));
            }
            d
        };
        assert_eq!(b.digest(), fresh(&b));
        b.flush_digest();
        assert_eq!(b.digest(), fresh(&b));

        // Slots are reused.
        b.insert(o(5, Side::Bid, 99, 1));
        assert_eq!(b.slab.len(), 4);
        assert_eq!(b.len(), 3);
        assert_eq!(b.clear().len(), 3);
        assert!(b.is_empty());
    }

    #[test]
    fn deferred_digest_matches_a_fresh_hash_under_churn() {
        // Many orders placed, reduced and cancelled within "blocks" of varying
        // size, including enough to take the parallel flush path.
        let fresh = |b: &Book| {
            let mut d = crate::commit::MultisetHash::default();
            for side in [Side::Bid, Side::Ask] {
                b.iter(side).for_each(|o| d.add(&crate::commit::order_hash(o)));
            }
            d
        };
        let mut b = Book::default();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let mut next = 1u64;
        for block in [10usize, 500, 20_000, 3, 9_000] {
            for _ in 0..block {
                match rnd(3) {
                    0 | 1 => {
                        let side = if rnd(2) == 0 { Side::Bid } else { Side::Ask };
                        b.insert(o(next, side, 100 + rnd(50), 1 + rnd(9)));
                        next += 1;
                    }
                    _ if !b.is_empty() => {
                        let id = 1 + rnd(next - 1);
                        if let Some(ord) = b.get(id).copied() {
                            if ord.remaining > 1 && rnd(2) == 0 {
                                b.reduce(id, 1);
                            } else {
                                b.remove(id);
                            }
                        }
                    }
                    _ => {}
                }
            }
            assert_eq!(b.digest(), fresh(&b), "before flush");
            b.flush_digest();
            assert_eq!(b.digest(), fresh(&b), "after flush");
        }
    }
}
