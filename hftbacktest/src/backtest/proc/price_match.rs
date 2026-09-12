use crate::{
    backtest::BacktestError,
    depth::MarketDepth,
    types::{OrdType, Order, PriceMatch, Side},
};

pub(crate) fn validate_price_match(
    order_type: OrdType,
    price_match: PriceMatch,
) -> Result<(), BacktestError> {
    match price_match {
        PriceMatch::None => Ok(()),
        PriceMatch::Unsupported => Err(BacktestError::InvalidOrderRequest),
        _ if order_type == OrdType::Limit => Ok(()),
        _ => Err(BacktestError::InvalidOrderRequest),
    }
}

pub(crate) fn resolve_price_match<MD: MarketDepth>(order: &Order, depth: &MD) -> Option<i64> {
    let (book_side, level) = match order.price_match {
        PriceMatch::None => return Some(order.price_tick),
        PriceMatch::Opponent => (opposite(order.side), 1),
        PriceMatch::Opponent5 => (opposite(order.side), 5),
        PriceMatch::Opponent10 => (opposite(order.side), 10),
        PriceMatch::Opponent20 => (opposite(order.side), 20),
        PriceMatch::Queue => (order.side, 1),
        PriceMatch::Queue5 => (order.side, 5),
        PriceMatch::Queue10 => (order.side, 10),
        PriceMatch::Queue20 => (order.side, 20),
        PriceMatch::Unsupported => return None,
    };

    price_level(depth, book_side, level)
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
        Side::None | Side::Unsupported => Side::Unsupported,
    }
}

fn price_level<MD: MarketDepth>(depth: &MD, side: Side, level: usize) -> Option<i64> {
    let mut remaining = level;
    let mut matched = None;
    let mut visit = |price_tick: i64, qty: f64| {
        if qty > 0.0 {
            remaining -= 1;
            if remaining == 0 {
                matched = Some(price_tick);
                return false;
            }
        }
        true
    };
    match side {
        Side::Buy => depth.for_each_bid_depth_from(depth.best_bid_tick(), &mut visit),
        Side::Sell => depth.for_each_ask_depth_from(depth.best_ask_tick(), &mut visit),
        Side::None | Side::Unsupported => return None,
    }
    matched
}
