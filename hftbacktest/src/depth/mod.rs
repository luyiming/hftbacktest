use rust_decimal::Decimal;

pub use btreemarketdepth::BTreeMarketDepth;

use crate::{prelude::Side, types::Event};

mod btreemarketdepth;

pub trait MarketDepth {
    fn depth_ready(&self) -> bool;
    fn mark_depth_ready(&mut self);
    fn best_bid(&self) -> Option<Decimal>;
    fn best_ask(&self) -> Option<Decimal>;
    fn best_bid_qty(&self) -> Decimal;
    fn best_ask_qty(&self) -> Decimal;
    fn bid_qty_at_price(&self, price: Decimal) -> Decimal;
    fn ask_qty_at_price(&self, price: Decimal) -> Decimal;

    fn for_each_ask_depth_from<F>(&self, start_price: Decimal, visitor: F)
    where
        F: FnMut(Decimal, Decimal) -> bool;

    fn for_each_bid_depth_from<F>(&self, start_price: Decimal, visitor: F)
    where
        F: FnMut(Decimal, Decimal) -> bool;
}

/// Result of updating one price level on one side of the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepthUpdate {
    pub level_price: Decimal,
    pub previous_best_price: Option<Decimal>,
    pub best_price: Option<Decimal>,
    pub previous_qty: Decimal,
    pub new_qty: Decimal,
    pub timestamp: i64,
}

pub trait L2MarketDepth {
    fn update_bid_depth(&mut self, price: Decimal, qty: Decimal, timestamp: i64) -> DepthUpdate;

    fn update_ask_depth(&mut self, price: Decimal, qty: Decimal, timestamp: i64) -> DepthUpdate;

    /// `clear_upto_price` is `None` when the entire selected side should be cleared.
    fn clear_depth(&mut self, side: Side, clear_upto_price: Option<Decimal>);
}

pub trait ApplySnapshot {
    fn apply_snapshot(&mut self, data: &[Event]);
    fn snapshot(&self) -> Vec<Event>;
}

pub trait L1MarketDepth {
    fn update_best_bid(&mut self, price: Decimal, qty: Decimal, timestamp: i64) -> DepthUpdate;

    fn update_best_ask(&mut self, price: Decimal, qty: Decimal, timestamp: i64) -> DepthUpdate;
}
