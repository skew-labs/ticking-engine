//! Price-level order book: a `BTreeMap` of levels, each a FIFO linked list
//! of orders in a slab. Insert is O(log levels), cancel by id is O(1) plus a
//! level removal when it empties, and the best price is O(log levels).

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
    /// Multiset hash of resting orders, kept current on every change.
    digest: crate::commit::MultisetHash,
}

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

    pub fn digest(&self) -> crate::commit::MultisetHash {
        self.digest
    }

    pub fn get(&self, id: OrderId) -> Option<&Order> {
        self.index.get(&id).map(|&i| &self.slab[i as usize].order)
    }

    /// Appends to the back of its price level.
    pub fn insert(&mut self, order: Order) {
        debug_assert!(!self.index.contains_key(&order.id));
        self.digest.add(&crate::commit::order_hash(&order));
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
        self.digest.add(&crate::commit::order_hash(&order));
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
        let idx = self.index.remove(&id)?;
        let Node { order, prev, next } = self.slab[idx as usize].clone();
        self.digest.sub(&crate::commit::order_hash(&order));
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
        let idx = self.index[&id];
        let order = &mut self.slab[idx as usize].order;
        self.digest.sub(&crate::commit::order_hash(order));
        order.remaining -= qty;
        self.digest.add(&crate::commit::order_hash(order));
        let (side, price) = (order.side, order.price);
        self.levels_mut(side).get_mut(&price).expect("level of live order").total -= qty;
    }

    /// Orders on `side` in priority order (best price first, then time).
    pub fn iter(&self, side: Side) -> impl Iterator<Item = &Order> + '_ {
        let levels: Box<dyn Iterator<Item = &Level>> = match side {
            Side::Bid => Box::new(self.bids.values().rev()),
            Side::Ask => Box::new(self.asks.values()),
        };
        levels.flat_map(move |l| {
            let mut cur = l.head;
            std::iter::from_fn(move || {
                if cur == NIL {
                    return None;
                }
                let n = &self.slab[cur as usize];
                cur = n.next;
                Some(&n.order)
            })
        })
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

        // Slots are reused.
        b.insert(o(5, Side::Bid, 99, 1));
        assert_eq!(b.slab.len(), 4);
        assert_eq!(b.len(), 3);
        assert_eq!(b.clear().len(), 3);
        assert!(b.is_empty());
    }
}
