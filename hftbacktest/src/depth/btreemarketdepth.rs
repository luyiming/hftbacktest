use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::{
    depth::{ApplySnapshot, DepthUpdate, L2MarketDepth, MarketDepth},
    prelude::Side,
    types::{BUY_EVENT, DEPTH_SNAPSHOT_EVENT, Event, SELL_EVENT},
};

/// Sparse level-2 order book keyed by exact prices.
#[derive(Clone, Debug, Default)]
pub struct BTreeMarketDepth {
    pub depth_ready: bool,
    pub timestamp: i64,
    pub bid_depth: BTreeMap<Decimal, Decimal>,
    pub ask_depth: BTreeMap<Decimal, Decimal>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
}

impl BTreeMarketDepth {
    pub fn new() -> Self {
        Self::default()
    }

    fn refresh_best_bid(&mut self) {
        self.best_bid = self.bid_depth.last_key_value().map(|(&price, _)| price);
    }

    fn refresh_best_ask(&mut self) {
        self.best_ask = self.ask_depth.first_key_value().map(|(&price, _)| price);
    }

    fn discard_crossed_asks(&mut self) {
        if let Some(best_bid) = self.best_bid {
            self.best_ask = self
                .ask_depth
                .range((
                    std::ops::Bound::Excluded(best_bid),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .map(|(&price, _)| price);
        }
    }

    fn discard_crossed_bids(&mut self) {
        if let Some(best_ask) = self.best_ask {
            self.best_bid = self
                .bid_depth
                .range(..best_ask)
                .next_back()
                .map(|(&price, _)| price);
        }
    }
}

impl L2MarketDepth for BTreeMarketDepth {
    fn update_bid_depth(&mut self, price: Decimal, qty: Decimal, timestamp: i64) -> DepthUpdate {
        let previous_best = self.best_bid;
        let previous_qty = self.bid_depth.get(&price).copied().unwrap_or_default();
        if qty.is_zero() {
            self.bid_depth.remove(&price);
        } else {
            self.bid_depth.insert(price, qty);
        }
        self.refresh_best_bid();
        self.discard_crossed_asks();
        self.timestamp = timestamp;
        (
            price,
            previous_best,
            self.best_bid,
            previous_qty,
            qty,
            timestamp,
        )
    }

    fn update_ask_depth(&mut self, price: Decimal, qty: Decimal, timestamp: i64) -> DepthUpdate {
        let previous_best = self.best_ask;
        let previous_qty = self.ask_depth.get(&price).copied().unwrap_or_default();
        if qty.is_zero() {
            self.ask_depth.remove(&price);
        } else {
            self.ask_depth.insert(price, qty);
        }
        self.refresh_best_ask();
        self.discard_crossed_bids();
        self.timestamp = timestamp;
        (
            price,
            previous_best,
            self.best_ask,
            previous_qty,
            qty,
            timestamp,
        )
    }

    fn clear_depth(&mut self, side: Side, clear_upto_price: Option<Decimal>) {
        match (side, clear_upto_price) {
            (Side::Buy, Some(limit)) => self.bid_depth.retain(|price, _| *price < limit),
            (Side::Sell, Some(limit)) => self.ask_depth.retain(|price, _| *price > limit),
            (Side::Buy, None) => self.bid_depth.clear(),
            (Side::Sell, None) => self.ask_depth.clear(),
        }
        self.refresh_best_bid();
        self.refresh_best_ask();
    }
}

impl MarketDepth for BTreeMarketDepth {
    fn depth_ready(&self) -> bool {
        self.depth_ready
    }

    fn mark_depth_ready(&mut self) {
        self.depth_ready = true;
    }

    fn best_bid(&self) -> Option<Decimal> {
        self.best_bid
    }

    fn best_ask(&self) -> Option<Decimal> {
        self.best_ask
    }

    fn best_bid_qty(&self) -> Decimal {
        self.best_bid
            .and_then(|price| self.bid_depth.get(&price).copied())
            .unwrap_or_default()
    }

    fn best_ask_qty(&self) -> Decimal {
        self.best_ask
            .and_then(|price| self.ask_depth.get(&price).copied())
            .unwrap_or_default()
    }

    fn bid_qty_at_price(&self, price: Decimal) -> Decimal {
        self.bid_depth.get(&price).copied().unwrap_or_default()
    }

    fn ask_qty_at_price(&self, price: Decimal) -> Decimal {
        self.ask_depth.get(&price).copied().unwrap_or_default()
    }

    fn for_each_ask_depth_from<F>(&self, start_price: Decimal, mut visitor: F)
    where
        F: FnMut(Decimal, Decimal) -> bool,
    {
        for (&price, &qty) in self.ask_depth.range(start_price..) {
            if !qty.is_zero() && !visitor(price, qty) {
                break;
            }
        }
    }

    fn for_each_bid_depth_from<F>(&self, start_price: Decimal, mut visitor: F)
    where
        F: FnMut(Decimal, Decimal) -> bool,
    {
        for (&price, &qty) in self.bid_depth.range(..=start_price).rev() {
            if !qty.is_zero() && !visitor(price, qty) {
                break;
            }
        }
    }
}

impl ApplySnapshot for BTreeMarketDepth {
    fn apply_snapshot(&mut self, data: &[Event]) {
        self.bid_depth.clear();
        self.ask_depth.clear();
        for event in data {
            if event.ev & BUY_EVENT == BUY_EVENT {
                self.bid_depth.insert(event.px, event.qty);
            } else if event.ev & SELL_EVENT == SELL_EVENT {
                self.ask_depth.insert(event.px, event.qty);
            }
        }
        self.refresh_best_bid();
        self.refresh_best_ask();
        self.mark_depth_ready();
    }

    fn snapshot(&self) -> Vec<Event> {
        self.bid_depth
            .iter()
            .map(|(&px, &qty)| Event {
                ev: DEPTH_SNAPSHOT_EVENT | BUY_EVENT,
                exch_ts: self.timestamp,
                local_ts: self.timestamp,
                px,
                qty,
            })
            .chain(self.ask_depth.iter().map(|(&px, &qty)| Event {
                ev: DEPTH_SNAPSHOT_EVENT | SELL_EVENT,
                exch_ts: self.timestamp,
                local_ts: self.timestamp,
                px,
                qty,
            }))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use crate::{
        depth::{BTreeMarketDepth, L2MarketDepth, MarketDepth},
        types::Side,
    };

    fn dec(value: i64, scale: u32) -> Decimal {
        Decimal::new(value, scale)
    }

    #[test]
    fn preserves_prices_that_do_not_share_a_tick_grid() {
        let mut depth = BTreeMarketDepth::new();
        depth.update_bid_depth(dec(10001, 2), dec(1, 0), 1);
        depth.update_bid_depth(dec(100005, 3), dec(2, 0), 2);
        assert_eq!(depth.best_bid(), Some(dec(10001, 2)));
        assert_eq!(depth.bid_qty_at_price(dec(100005, 3)), dec(2, 0));
    }

    #[test]
    fn clears_only_prices_inside_the_requested_side_range() {
        let mut depth = BTreeMarketDepth::new();
        depth.update_ask_depth(dec(100, 0), dec(1, 0), 0);
        depth.update_ask_depth(dec(150, 0), dec(1, 0), 0);
        depth.clear_depth(Side::Sell, Some(dec(120, 0)));
        assert_eq!(depth.best_ask(), Some(dec(150, 0)));
    }
}
