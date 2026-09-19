use crate::{
    backtest::{
        BacktestError,
        rules::{TickSizeError, TickSizeSchedule},
    },
    depth::MarketDepth,
    types::{OrdType, Order, PriceMatch, Side},
};
use rust_decimal::Decimal;

pub(crate) fn validate_price_match(
    order_type: OrdType,
    price_match: PriceMatch,
) -> Result<(), BacktestError> {
    match price_match {
        PriceMatch::None => Ok(()),
        _ if order_type == OrdType::Limit => Ok(()),
        _ => Err(BacktestError::InvalidOrderRequest),
    }
}

pub(crate) fn price_satisfies_rule(
    order: &Order,
    timestamp: i64,
    schedule: &TickSizeSchedule,
) -> Result<bool, TickSizeError> {
    if order.order_type == OrdType::Limit && order.price_match == PriceMatch::None {
        schedule.accepts(timestamp, order.price)
    } else {
        // PriceMatch is an instruction, not a user-selected price. Its resolved book price remains
        // valid even when it came from a depth level created under an older tick-size rule.
        Ok(true)
    }
}

pub(crate) fn resolve_price_match<MD: MarketDepth>(order: &Order, depth: &MD) -> Option<Decimal> {
    let (book_side, level) = match order.price_match {
        PriceMatch::None => return Some(order.price),
        PriceMatch::Opponent => (opposite(order.side), 1),
        PriceMatch::Opponent5 => (opposite(order.side), 5),
        PriceMatch::Opponent10 => (opposite(order.side), 10),
        PriceMatch::Opponent20 => (opposite(order.side), 20),
        PriceMatch::Queue => (order.side, 1),
        PriceMatch::Queue5 => (order.side, 5),
        PriceMatch::Queue10 => (order.side, 10),
        PriceMatch::Queue20 => (order.side, 20),
    };

    price_level(depth, book_side, level)
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

fn price_level<MD: MarketDepth>(depth: &MD, side: Side, level: usize) -> Option<Decimal> {
    let mut remaining = level;
    let mut matched = None;
    let mut visit = |price: Decimal, qty: Decimal| {
        if qty > Decimal::ZERO {
            remaining -= 1;
            if remaining == 0 {
                matched = Some(price);
                return false;
            }
        }
        true
    };
    match side {
        Side::Buy => depth.for_each_bid_depth_from(depth.best_bid()?, &mut visit),
        Side::Sell => depth.for_each_ask_depth_from(depth.best_ask()?, &mut visit),
    }
    matched
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::price_satisfies_rule;
    use crate::{
        backtest::rules::{TickSizeChange, TickSizeSchedule},
        types::{OrdType, Order, PriceMatch, Side, TimeInForce},
    };

    #[test]
    fn price_match_is_exempt_from_explicit_tick_validation() {
        let schedule = TickSizeSchedule::new(vec![
            TickSizeChange {
                effective_from: 0,
                tick_size: Decimal::new(1, 2),
            },
            TickSizeChange {
                effective_from: 10,
                tick_size: Decimal::new(1, 1),
            },
        ])
        .unwrap();
        let mut order = Order::new(
            1,
            Decimal::new(10005, 2),
            Decimal::ONE,
            Side::Buy,
            OrdType::Limit,
            TimeInForce::GTC,
        );
        assert!(price_satisfies_rule(&order, 9, &schedule).unwrap());
        assert!(!price_satisfies_rule(&order, 10, &schedule).unwrap());
        order.price_match = PriceMatch::Opponent;
        assert!(price_satisfies_rule(&order, 10, &schedule).unwrap());
        order.price_match = PriceMatch::None;
        order.order_type = OrdType::Market;
        assert!(price_satisfies_rule(&order, 10, &schedule).unwrap());
    }
}
