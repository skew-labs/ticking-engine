//! Fixed-capacity price-time priority book side.

use crate::error::{EngineError, Result};
use crate::state::{BOOK_CAPACITY, BookSide, Order, Side};

/// True if `a` has strictly higher priority than `b` on `side`.
pub fn better(side: Side, a: &Order, b: &Order) -> bool {
    let price_better = match side {
        Side::Bid => a.price > b.price,
        Side::Ask => a.price < b.price,
    };
    price_better || (a.price == b.price && a.order_id < b.order_id)
}

/// True if a taker on `side` at `limit` trades against a resting `price`.
pub fn crosses(side: Side, limit: u64, price: u64) -> bool {
    match side {
        Side::Bid => limit >= price,
        Side::Ask => limit <= price,
    }
}

impl BookSide {
    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn orders(&self) -> &[Order] {
        &self.orders[..self.len()]
    }

    pub fn best(&self) -> Option<&Order> {
        self.orders().last()
    }

    pub fn best_mut(&mut self) -> Option<&mut Order> {
        let n = self.len();
        if n == 0 { None } else { Some(&mut self.orders[n - 1]) }
    }

    pub fn pop_best(&mut self) -> Option<Order> {
        let n = self.len();
        if n == 0 {
            return None;
        }
        let o = self.orders[n - 1];
        self.orders[n - 1] = Order::default();
        self.count -= 1;
        Some(o)
    }

    pub fn find(&self, order_id: u64) -> Option<usize> {
        self.orders().iter().position(|o| o.order_id == order_id)
    }

    pub fn remove_at(&mut self, idx: usize) -> Order {
        let n = self.len();
        let o = self.orders[idx];
        self.orders.copy_within(idx + 1..n, idx);
        self.orders[n - 1] = Order::default();
        self.count -= 1;
        o
    }

    /// Inserts by priority. When full, the worst order is evicted if the new
    /// one beats it and returned so the caller can release its margin.
    pub fn insert(&mut self, side: Side, order: Order) -> Result<Option<Order>> {
        let mut evicted = None;
        if self.len() == BOOK_CAPACITY {
            if !better(side, &order, &self.orders[0]) {
                return Err(EngineError::BookFull);
            }
            evicted = Some(self.remove_at(0));
        }
        let n = self.len();
        let at = self.orders[..n].partition_point(|o| !better(side, o, &order));
        self.orders.copy_within(at..n, at + 1);
        self.orders[at] = order;
        self.count += 1;
        Ok(evicted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    fn o(id: u64, price: u64) -> Order {
        Order { order_id: id, price, base_lots: 1, ..Order::default() }
    }

    #[test]
    fn bids_sorted_worst_to_best() {
        let mut s = BookSide::zeroed();
        for (id, p) in [(1, 100), (2, 105), (3, 100), (4, 95)] {
            s.insert(Side::Bid, o(id, p)).unwrap();
        }
        let ids: Vec<u64> = s.orders().iter().map(|o| o.order_id).collect();
        // worst..best: 95, 100 (id 3), 100 (id 1), 105
        assert_eq!(ids, vec![4, 3, 1, 2]);
        assert_eq!(s.pop_best().unwrap().order_id, 2);
        assert_eq!(s.best().unwrap().order_id, 1);
    }

    #[test]
    fn asks_sorted_worst_to_best() {
        let mut s = BookSide::zeroed();
        for (id, p) in [(1, 100), (2, 105), (3, 100), (4, 95)] {
            s.insert(Side::Ask, o(id, p)).unwrap();
        }
        let ids: Vec<u64> = s.orders().iter().map(|o| o.order_id).collect();
        assert_eq!(ids, vec![2, 3, 1, 4]);
    }

    #[test]
    fn full_book_evicts_worst_only_for_better_orders() {
        let mut s = BookSide::zeroed();
        for i in 0..BOOK_CAPACITY as u64 {
            s.insert(Side::Bid, o(i + 1, 1_000 + i)).unwrap();
        }
        assert_eq!(s.insert(Side::Bid, o(9_999, 1_000)), Err(EngineError::BookFull));
        let ev = s.insert(Side::Bid, o(10_000, 5_000)).unwrap().unwrap();
        assert_eq!(ev.price, 1_000);
        assert_eq!(s.len(), BOOK_CAPACITY);
        assert_eq!(s.best().unwrap().order_id, 10_000);
    }

    #[test]
    fn remove_keeps_order() {
        let mut s = BookSide::zeroed();
        for (id, p) in [(1, 100), (2, 101), (3, 102)] {
            s.insert(Side::Bid, o(id, p)).unwrap();
        }
        let idx = s.find(2).unwrap();
        s.remove_at(idx);
        let ids: Vec<u64> = s.orders().iter().map(|o| o.order_id).collect();
        assert_eq!(ids, vec![1, 3]);
    }
}
