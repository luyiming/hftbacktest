mod local;
mod nopartialfillexchange;
mod partialfillexchange;
mod price_match;

use rust_decimal::Decimal;
use std::collections::HashMap;

pub use local::Local;
pub use nopartialfillexchange::NoPartialFillExchange;
pub use partialfillexchange::PartialFillExchange;

use crate::{
    backtest::{
        BacktestError,
        proc::price_match::{price_satisfies_rule, resolve_price_match},
        rules::TickSizeSchedule,
        snapshot::{SnapshotContext, SnapshotError},
    },
    depth::MarketDepth,
    prelude::{
        Event, OrdType, Order, OrderFill, OrderId, OrderRequest, OrderStatus, PriceMatch,
        RequestResult, Side, StateValues, TimeInForce,
    },
};

#[derive(Clone)]
struct RestingOrder<S> {
    order: Order,
    queue_state: S,
}

#[derive(Clone, Copy)]
struct OrderAmendment {
    price: Decimal,
    price_match: PriceMatch,
    qty: Decimal,
}

enum ModifyPlan {
    Reject,
    Terminate(OrderStatus),
    Reenter(Order),
}

enum ModifyAck {
    Accepted { order: Order, fills: Vec<OrderFill> },
    Rejected { current: Option<Order> },
}

fn plan_modify<MD>(
    existing: &Order,
    amendment: OrderAmendment,
    timestamp: i64,
    depth: &MD,
    tick_sizes: &TickSizeSchedule,
) -> Result<ModifyPlan, BacktestError>
where
    MD: MarketDepth,
{
    if amendment.qty <= Decimal::ZERO {
        return Ok(ModifyPlan::Reject);
    }

    let mut candidate = existing.clone();
    let price_instruction_changed =
        candidate.price != amendment.price || candidate.price_match != amendment.price_match;
    candidate.price = amendment.price;
    candidate.price_match = amendment.price_match;
    candidate.qty = amendment.qty;

    if price_instruction_changed && !price_satisfies_rule(&candidate, timestamp, tick_sizes)? {
        return Ok(ModifyPlan::Reject);
    }
    let Some(price) = resolve_price_match(&candidate, depth) else {
        return Ok(ModifyPlan::Reject);
    };
    candidate.price = price;

    if existing.filled > Decimal::ZERO && candidate.qty <= existing.filled {
        return Ok(ModifyPlan::Terminate(OrderStatus::Canceled));
    }
    let crosses_book = candidate.order_type == OrdType::Limit
        && candidate.time_in_force == TimeInForce::GTX
        && match candidate.side {
            Side::Buy => depth.best_ask().is_some_and(|ask| candidate.price >= ask),
            Side::Sell => depth.best_bid().is_some_and(|bid| candidate.price <= bid),
        };
    if crosses_book {
        return Ok(ModifyPlan::Terminate(OrderStatus::Expired));
    }

    Ok(ModifyPlan::Reenter(candidate))
}

/// Provides local-specific interaction.
pub trait LocalProcessor<MD>: Processor
where
    MD: MarketDepth,
{
    /// Copies local simulation state, reconnecting its buses through the branch context.
    /// The default explicitly rejects processors without snapshot support.
    fn snapshot_local(
        &self,
        _context: &mut SnapshotContext,
    ) -> Result<Box<dyn LocalProcessor<MD>>, SnapshotError> {
        Err(SnapshotError::Unsupported("local processor"))
    }

    /// Submits a new order.
    ///
    /// * `order_id` - The unique order ID; there should not be any existing order with the same ID
    ///   on both local and exchange sides.
    /// * `price` - Order price.
    /// * `price_match` - Exchange-side price matching mode. A non-`None` mode supersedes `price`.
    /// * `qty` - Quantity to buy.
    /// * `order_type` - Available [`OrdType`] options vary depending on the exchange model. See to
    ///   the exchange model for details.
    /// * `time_in_force` - Available [`TimeInForce`] options vary depending on the exchange model.
    ///   See to the exchange model for details.
    /// * `current_timestamp` - The current backtesting timestamp.
    #[allow(clippy::too_many_arguments)]
    fn submit_order(
        &mut self,
        order_id: OrderId,
        side: Side,
        price: Decimal,
        price_match: PriceMatch,
        qty: Decimal,
        order_type: OrdType,
        time_in_force: TimeInForce,
        current_timestamp: i64,
    ) -> Result<(), BacktestError>;

    /// Modifies an open order.
    ///
    /// * `order_id` - Order ID to modify.
    /// * `price` - Order price.
    /// * `price_match` - Exchange-side price matching mode. A non-`None` mode supersedes `price`.
    /// * `qty` - New total order quantity for L2 backtesting, including the cumulative executed
    ///   quantity.
    /// * `current_timestamp` - The current backtesting timestamp.
    fn modify(
        &mut self,
        order_id: OrderId,
        price: Decimal,
        price_match: PriceMatch,
        qty: Decimal,
        current_timestamp: i64,
    ) -> Result<(), BacktestError>;

    /// Cancels an open order.
    ///
    /// * `order_id` - Order ID to cancel.
    /// * `current_timestamp` - The current backtesting timestamp.
    fn cancel(&mut self, order_id: OrderId, current_timestamp: i64) -> Result<(), BacktestError>;

    /// Clears orders that are no longer open and have no pending request.
    fn clear_inactive_orders(&mut self);

    /// Returns the position you currently hold.
    fn position(&self) -> Decimal;

    /// Returns the state's values such as balance, fee, and so on.
    fn state_values(&self) -> &StateValues;

    /// Returns the [`MarketDepth`].
    fn depth(&self) -> &MD;

    /// Returns a hash map of order IDs and their corresponding [`Order`]s.
    fn orders(&self) -> &HashMap<OrderId, Order>;

    /// Returns the pending request for an order, if one exists.
    fn pending_order_request(&self, order_id: OrderId) -> Option<&OrderRequest>;

    /// Returns the most recent completed request result for an order.
    fn last_order_request_result(&self, order_id: OrderId) -> Option<RequestResult>;

    /// Returns the last market trades.
    fn last_trades(&self) -> &[Event];

    /// Clears the last market trades from the buffer.
    fn clear_last_trades(&mut self);

    /// Returns the last feed's exchange timestamp and local receipt timestamp.
    fn feed_latency(&self) -> Option<(i64, i64)>;

    /// Returns the last order's request timestamp, exchange timestamp, and response receipt
    /// timestamp.
    fn order_latency(&self) -> Option<(i64, i64, i64)>;
}

impl<P: Processor + ?Sized> Processor for Box<P> {
    fn snapshot_processor(
        &self,
        context: &mut SnapshotContext,
    ) -> Result<Box<dyn Processor>, SnapshotError> {
        P::snapshot_processor(self, context)
    }

    fn event_seen_timestamp(&self, event: &Event) -> Option<i64> {
        P::event_seen_timestamp(self, event)
    }

    fn process(&mut self, event: &Event) -> Result<(), BacktestError> {
        P::process(self, event)
    }

    fn process_recv_order(
        &mut self,
        timestamp: i64,
        wait_resp_order_id: Option<OrderId>,
    ) -> Result<bool, BacktestError> {
        P::process_recv_order(self, timestamp, wait_resp_order_id)
    }

    fn earliest_recv_order_timestamp(&self) -> i64 {
        P::earliest_recv_order_timestamp(self)
    }

    fn earliest_send_order_timestamp(&self) -> i64 {
        P::earliest_send_order_timestamp(self)
    }
}
/// Processes the historical feed data and the order interaction.
pub trait Processor {
    /// Copies processor state without advancing it or sharing mutable state with the source.
    /// Connected processors must use the same context to reconstruct branch-local order buses.
    fn snapshot_processor(
        &self,
        _context: &mut SnapshotContext,
    ) -> Result<Box<dyn Processor>, SnapshotError> {
        Err(SnapshotError::Unsupported("exchange processor"))
    }

    /// The time of an event as seen by this [Processor]. For a local event processor this will
    /// be the timestamp an event was seen at locally, and for an exchange processor this will
    /// be the timestamp an event was generated at on the exchange.
    ///
    /// `None` should be returned if this processor wouldn't have seen this event (i.e. it only
    /// occurred remotely).
    fn event_seen_timestamp(&self, event: &Event) -> Option<i64>;

    /// Process an event and advance the state of this processor.
    fn process(&mut self, event: &Event) -> Result<(), BacktestError>;

    /// Processes an order upon receipt. This is invoked when the backtesting time reaches the order
    /// receipt timestamp.
    /// Returns Ok(true) if the order with `wait_resp_order_id` is received and processed.
    fn process_recv_order(
        &mut self,
        timestamp: i64,
        wait_resp_order_id: Option<OrderId>,
    ) -> Result<bool, BacktestError>;

    /// Returns the foremost timestamp at which an order is to be received by this processor.
    fn earliest_recv_order_timestamp(&self) -> i64;

    /// Returns the foremost timestamp at which an order sent by this processor is to be received by
    /// the corresponding processor.
    fn earliest_send_order_timestamp(&self) -> i64;
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::{ModifyPlan, OrderAmendment, plan_modify};
    use crate::{
        backtest::rules::{TickSizeChange, TickSizeSchedule},
        depth::{BTreeMarketDepth, L2MarketDepth},
        types::{OrdType, Order, OrderStatus, PriceMatch, Side, TimeInForce},
    };

    fn tick_sizes() -> TickSizeSchedule {
        TickSizeSchedule::new(vec![
            TickSizeChange {
                effective_from: 0,
                tick_size: Decimal::new(1, 2),
            },
            TickSizeChange {
                effective_from: 10,
                tick_size: Decimal::ONE,
            },
        ])
        .expect("test tick size schedule should be valid")
    }

    #[test]
    fn switching_from_price_match_to_explicit_price_revalidates_tick_size() {
        let mut existing = Order::new(
            1,
            Decimal::new(10005, 2),
            Decimal::ONE,
            Side::Buy,
            OrdType::Limit,
            TimeInForce::GTC,
        );
        existing.price_match = PriceMatch::Queue;

        let plan = plan_modify(
            &existing,
            OrderAmendment {
                price: existing.price,
                price_match: PriceMatch::None,
                qty: existing.qty,
            },
            10,
            &BTreeMarketDepth::new(),
            &tick_sizes(),
        )
        .expect("modify should be planned");

        assert!(matches!(plan, ModifyPlan::Reject));
    }

    #[test]
    fn crossing_gtx_modify_expires_on_both_sides() {
        let mut depth = BTreeMarketDepth::new();
        depth.update_bid_depth(Decimal::from(99), Decimal::ONE, 0);
        depth.update_ask_depth(Decimal::from(101), Decimal::ONE, 0);

        for (side, resting_price, crossing_price) in [
            (Side::Buy, Decimal::from(100), Decimal::from(101)),
            (Side::Sell, Decimal::from(102), Decimal::from(99)),
        ] {
            let existing = Order::new(
                1,
                resting_price,
                Decimal::ONE,
                side,
                OrdType::Limit,
                TimeInForce::GTX,
            );
            let plan = plan_modify(
                &existing,
                OrderAmendment {
                    price: crossing_price,
                    price_match: PriceMatch::None,
                    qty: existing.qty,
                },
                0,
                &depth,
                &tick_sizes(),
            )
            .expect("modify should be planned");

            assert!(matches!(plan, ModifyPlan::Terminate(OrderStatus::Expired)));
        }
    }
}
