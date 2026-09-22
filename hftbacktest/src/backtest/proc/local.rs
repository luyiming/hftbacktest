use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::{
    backtest::{
        BacktestError,
        assettype::AssetType,
        models::{FeeModel, LatencyModel},
        order_bus::LocalToExch,
        proc::{LocalProcessor, Processor, price_match::validate_price_match},
        snapshot::{LocalSnapshotFn, SnapshotContext, SnapshotError, SnapshotState},
        state::State,
    },
    depth::{L2MarketDepth, MarketDepth},
    types::{
        Event, LOCAL_ASK_DEPTH_CLEAR_EVENT, LOCAL_ASK_DEPTH_EVENT, LOCAL_ASK_DEPTH_SNAPSHOT_EVENT,
        LOCAL_BID_DEPTH_CLEAR_EVENT, LOCAL_BID_DEPTH_EVENT, LOCAL_BID_DEPTH_SNAPSHOT_EVENT,
        LOCAL_DEPTH_CLEAR_EVENT, LOCAL_EVENT, LOCAL_TRADE_EVENT, NewOrder, OrdType, Order, OrderId,
        OrderRequest, PriceMatch, RequestResult, Side, StateValues, TimeInForce,
    },
};

/// The local model.
pub struct Local<AT, LM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    MD: MarketDepth,
    FM: FeeModel,
{
    orders: HashMap<OrderId, Order>,
    pending_requests: HashMap<OrderId, OrderRequest>,
    last_request_results: HashMap<OrderId, RequestResult>,
    next_request_id: u64,
    order_l2e: LocalToExch<LM>,
    depth: MD,
    state: State<AT, FM>,
    trades: Vec<Event>,
    last_feed_latency: Option<(i64, i64)>,
    last_order_latency: Option<(i64, i64, i64)>,
    snapshot_fn: Option<LocalSnapshotFn<Self, MD>>,
}

impl<AT, LM, MD, FM> Local<AT, LM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    MD: MarketDepth,
    FM: FeeModel,
{
    /// Constructs an instance of `Local`.
    pub fn new(
        depth: MD,
        state: State<AT, FM>,
        last_trades_cap: usize,
        order_l2e: LocalToExch<LM>,
    ) -> Self {
        Self {
            orders: Default::default(),
            pending_requests: Default::default(),
            last_request_results: Default::default(),
            next_request_id: 0,
            order_l2e,
            depth,
            state,
            trades: Vec::with_capacity(last_trades_cap),
            last_feed_latency: None,
            last_order_latency: None,
            snapshot_fn: None,
        }
    }

    pub(crate) fn enable_snapshot(mut self) -> Self
    where
        AT: SnapshotState + 'static,
        LM: SnapshotState + 'static,
        MD: L2MarketDepth + SnapshotState + 'static,
        FM: SnapshotState + 'static,
    {
        self.snapshot_fn = Some(|source, context| {
            // Capacity is the retention limit, including when the trade buffer is empty.
            let mut trades = Vec::with_capacity(source.trades.capacity());
            trades.extend_from_slice(&source.trades);
            Box::new(Self {
                orders: source.orders.clone(),
                pending_requests: source.pending_requests.clone(),
                last_request_results: source.last_request_results.clone(),
                next_request_id: source.next_request_id,
                order_l2e: source.order_l2e.snapshot(context),
                depth: source.depth.clone(),
                state: source.state.clone(),
                trades,
                last_feed_latency: source.last_feed_latency,
                last_order_latency: source.last_order_latency,
                snapshot_fn: source.snapshot_fn,
            })
        });
        self
    }

    pub fn process_recv_order_<const USE_HANDLER: bool, Handler>(
        &mut self,
        timestamp: i64,
        wait_resp_order_id: Option<OrderId>,
        mut handler: Handler,
    ) -> Result<bool, BacktestError>
    where
        Handler: FnMut(&Order),
    {
        let mut wait_resp_order_received = false;
        while let Some(update) = self.order_l2e.receive(timestamp) {
            if let Some(wait_resp_order_id) = wait_resp_order_id
                && update.order_id == wait_resp_order_id
                && update.request_result.is_some()
            {
                wait_resp_order_received = true;
            }

            let pending = update.request_result.and_then(|result| {
                self.pending_requests
                    .get(&update.order_id)
                    .filter(|request| request.request_id() == result.request_id)
                    .cloned()
            });
            if let Some(request) = &pending {
                self.last_order_latency =
                    Some((request.local_timestamp(), update.exch_timestamp, timestamp));
            }

            if update.state.is_some() && !self.orders.contains_key(&update.order_id) {
                let Some(OrderRequest::New { order: new, .. }) = pending.as_ref() else {
                    return Err(BacktestError::InvalidOrderRequest);
                };
                let mut order = Order::new(
                    new.order_id,
                    new.price,
                    new.qty,
                    new.side,
                    new.order_type,
                    new.time_in_force,
                );
                order.price_match = new.price_match;
                self.orders.insert(update.order_id, order);
            }

            if let Some(order) = self.orders.get_mut(&update.order_id) {
                for fill in update.fills {
                    self.state.apply_fill(order.side, &fill);
                    order.apply_fill(fill);
                }
                if let Some(state) = &update.state {
                    order.apply_state(state, update.exch_timestamp);
                }
                if USE_HANDLER {
                    handler(order);
                }
            }
            if let Some(result) = update.request_result {
                self.last_request_results.insert(update.order_id, result);
                self.pending_requests.remove(&update.order_id);
            }
        }
        Ok(wait_resp_order_received)
    }

    fn next_request_id(&mut self) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .expect("order request id should not overflow");
        request_id
    }
}

impl<AT, LM, MD, FM> LocalProcessor<MD> for Local<AT, LM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    MD: MarketDepth + L2MarketDepth,
    FM: FeeModel,
{
    fn snapshot_local(
        &self,
        context: &mut SnapshotContext,
    ) -> Result<Box<dyn LocalProcessor<MD>>, SnapshotError> {
        let snapshot = self
            .snapshot_fn
            .ok_or(SnapshotError::Unsupported("L2 local processor"))?;
        Ok(snapshot(self, context))
    }

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
    ) -> Result<(), BacktestError> {
        validate_price_match(order_type, price_match)?;
        if self.orders.contains_key(&order_id) || self.pending_requests.contains_key(&order_id) {
            return Err(BacktestError::OrderIdExist);
        }

        let request = OrderRequest::New {
            request_id: self.next_request_id(),
            local_timestamp: current_timestamp,
            order: NewOrder {
                order_id,
                price,
                price_match,
                qty,
                side,
                time_in_force,
                order_type,
            },
        };
        self.pending_requests.insert(order_id, request.clone());
        self.order_l2e.request(request);

        Ok(())
    }

    fn modify(
        &mut self,
        order_id: OrderId,
        price: Decimal,
        price_match: PriceMatch,
        qty: Decimal,
        current_timestamp: i64,
    ) -> Result<(), BacktestError> {
        let order = self
            .orders
            .get(&order_id)
            .ok_or(BacktestError::OrderNotFound)?;

        if self.pending_requests.contains_key(&order_id) {
            return Err(BacktestError::OrderRequestInProcess);
        }
        validate_price_match(order.order_type, price_match)?;
        if !order.active() {
            return Err(BacktestError::InvalidOrderStatus);
        }

        let request = OrderRequest::Modify {
            request_id: self.next_request_id(),
            local_timestamp: current_timestamp,
            order_id,
            price,
            price_match,
            qty,
        };
        self.pending_requests.insert(order_id, request.clone());
        self.order_l2e.request(request);

        Ok(())
    }

    fn cancel(&mut self, order_id: OrderId, current_timestamp: i64) -> Result<(), BacktestError> {
        let order = self
            .orders
            .get(&order_id)
            .ok_or(BacktestError::OrderNotFound)?;

        if self.pending_requests.contains_key(&order_id) {
            return Err(BacktestError::OrderRequestInProcess);
        }
        if !order.active() {
            return Err(BacktestError::InvalidOrderStatus);
        }

        let request = OrderRequest::Cancel {
            request_id: self.next_request_id(),
            local_timestamp: current_timestamp,
            order_id,
        };
        self.pending_requests.insert(order_id, request.clone());
        self.order_l2e.request(request);

        Ok(())
    }

    fn clear_inactive_orders(&mut self) {
        self.orders.retain(|order_id, order| {
            order.active() || self.pending_requests.contains_key(order_id)
        })
    }

    fn position(&self) -> Decimal {
        self.state.values().position
    }

    fn state_values(&self) -> &StateValues {
        self.state.values()
    }

    fn depth(&self) -> &MD {
        &self.depth
    }

    fn orders(&self) -> &HashMap<u64, Order> {
        &self.orders
    }

    fn pending_order_request(&self, order_id: OrderId) -> Option<&OrderRequest> {
        self.pending_requests.get(&order_id)
    }

    fn last_order_request_result(&self, order_id: OrderId) -> Option<RequestResult> {
        self.last_request_results.get(&order_id).copied()
    }

    fn last_trades(&self) -> &[Event] {
        self.trades.as_slice()
    }

    fn clear_last_trades(&mut self) {
        self.trades.clear();
    }

    fn feed_latency(&self) -> Option<(i64, i64)> {
        self.last_feed_latency
    }

    fn order_latency(&self) -> Option<(i64, i64, i64)> {
        self.last_order_latency
    }
}

impl<AT, LM, MD, FM> Processor for Local<AT, LM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    MD: MarketDepth + L2MarketDepth,
    FM: FeeModel,
{
    fn event_seen_timestamp(&self, event: &Event) -> Option<i64> {
        event.is(LOCAL_EVENT).then_some(event.local_ts)
    }

    fn process(&mut self, ev: &Event) -> Result<(), BacktestError> {
        // Processes a depth event
        if ev.is(LOCAL_BID_DEPTH_CLEAR_EVENT) {
            self.depth.clear_depth(Side::Buy, Some(ev.px));
        } else if ev.is(LOCAL_ASK_DEPTH_CLEAR_EVENT) {
            self.depth.clear_depth(Side::Sell, Some(ev.px));
        } else if ev.is(LOCAL_DEPTH_CLEAR_EVENT) {
            self.depth.clear_depth(Side::Buy, None);
            self.depth.clear_depth(Side::Sell, None);
        } else if ev.is(LOCAL_BID_DEPTH_EVENT) || ev.is(LOCAL_BID_DEPTH_SNAPSHOT_EVENT) {
            self.depth.update_bid_depth(ev.px, ev.qty, ev.local_ts);
            if ev.is(LOCAL_BID_DEPTH_SNAPSHOT_EVENT) {
                self.depth.mark_depth_ready();
            }
        } else if ev.is(LOCAL_ASK_DEPTH_EVENT) || ev.is(LOCAL_ASK_DEPTH_SNAPSHOT_EVENT) {
            self.depth.update_ask_depth(ev.px, ev.qty, ev.local_ts);
            if ev.is(LOCAL_ASK_DEPTH_SNAPSHOT_EVENT) {
                self.depth.mark_depth_ready();
            }
        }
        // Processes a trade event
        else if ev.is(LOCAL_TRADE_EVENT) && self.trades.capacity() > 0 {
            if self.trades.len() == self.trades.capacity() {
                self.trades.remove(0);
            }
            self.trades.push(ev.clone());
        }

        // Stores the current feed latency
        self.last_feed_latency = Some((ev.exch_ts, ev.local_ts));

        Ok(())
    }

    fn process_recv_order(
        &mut self,
        timestamp: i64,
        wait_resp_order_id: Option<OrderId>,
    ) -> Result<bool, BacktestError> {
        self.process_recv_order_::<false, _>(timestamp, wait_resp_order_id, |_| {})
    }

    fn earliest_recv_order_timestamp(&self) -> i64 {
        self.order_l2e
            .earliest_recv_order_timestamp()
            .unwrap_or(i64::MAX)
    }

    fn earliest_send_order_timestamp(&self) -> i64 {
        self.order_l2e
            .earliest_send_order_timestamp()
            .unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backtest::{
            assettype::LinearAsset,
            models::{CommonFees, ConstantLatency, RiskAdverseQueueModel, TradingValueFeeModel},
            order_bus::order_bus,
            proc::PartialFillExchange,
            rules::{TickSizeChange, TickSizeSchedule},
        },
        depth::BTreeMarketDepth,
        types::{EXCH_ASK_DEPTH_EVENT, EXCH_SELL_TRADE_EVENT, OrderStatus, RequestOutcome},
    };

    #[test]
    fn preserves_each_fill_in_a_multilevel_order_update() {
        let (order_e2l, order_l2e) = order_bus(ConstantLatency::new(0, 0));
        let state = || {
            State::new(
                LinearAsset::new(1.0),
                TradingValueFeeModel::new(CommonFees::new(0.0, 0.0)),
            )
        };
        let mut local = Local::new(BTreeMarketDepth::new(), state(), 0, order_l2e);
        let schedule = TickSizeSchedule::new(vec![TickSizeChange {
            effective_from: 0,
            tick_size: Decimal::ONE,
        }])
        .expect("test tick size schedule should be valid");
        let mut exchange = PartialFillExchange::new(
            BTreeMarketDepth::new(),
            state(),
            RiskAdverseQueueModel::new(),
            order_e2l,
            schedule,
        );
        for (price, qty) in [(100, 2), (101, 1)] {
            exchange
                .process(&Event {
                    ev: EXCH_ASK_DEPTH_EVENT,
                    exch_ts: 0,
                    local_ts: 0,
                    px: Decimal::from(price),
                    qty: Decimal::from(qty),
                })
                .expect("depth event should be processed");
        }

        local
            .submit_order(
                1,
                Side::Buy,
                Decimal::from(101),
                PriceMatch::None,
                Decimal::from(3),
                OrdType::Limit,
                TimeInForce::IOC,
                0,
            )
            .expect("order should be submitted");
        exchange
            .process_recv_order(0, None)
            .expect("exchange should process the request");
        local
            .process_recv_order(0, Some(1))
            .expect("local should process the update");

        let order = &local.orders()[&1];
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(order.filled, Decimal::from(3));
        assert_eq!(
            order
                .fills
                .iter()
                .map(|fill| (fill.price, fill.qty, fill.is_maker))
                .collect::<Vec<_>>(),
            vec![
                (Decimal::from(100), Decimal::from(2), false),
                (Decimal::from(101), Decimal::ONE, false),
            ]
        );
        assert!(local.pending_order_request(1).is_none());
        assert_eq!(
            local
                .last_order_request_result(1)
                .expect("request result should be retained")
                .outcome,
            RequestOutcome::Accepted
        );
        assert_eq!(local.state_values().num_trades, 2);
    }

    #[test]
    fn one_order_retains_maker_and_taker_fills_across_modify() {
        let (order_e2l, order_l2e) = order_bus(ConstantLatency::new(0, 0));
        let state = || {
            State::new(
                LinearAsset::new(1.0),
                TradingValueFeeModel::new(CommonFees::new(0.0, 0.0)),
            )
        };
        let mut local = Local::new(BTreeMarketDepth::new(), state(), 0, order_l2e);
        let schedule = TickSizeSchedule::new(vec![TickSizeChange {
            effective_from: 0,
            tick_size: Decimal::ONE,
        }])
        .expect("test tick size schedule should be valid");
        let mut exchange = PartialFillExchange::new(
            BTreeMarketDepth::new(),
            state(),
            RiskAdverseQueueModel::new(),
            order_e2l,
            schedule,
        );
        exchange
            .process(&Event {
                ev: EXCH_ASK_DEPTH_EVENT,
                exch_ts: 0,
                local_ts: 0,
                px: Decimal::from(101),
                qty: Decimal::TWO,
            })
            .expect("ask should be processed");
        local
            .submit_order(
                7,
                Side::Buy,
                Decimal::from(100),
                PriceMatch::None,
                Decimal::from(3),
                OrdType::Limit,
                TimeInForce::GTC,
                0,
            )
            .expect("maker order should be submitted");
        exchange
            .process_recv_order(0, None)
            .expect("exchange should accept maker order");
        local
            .process_recv_order(0, Some(7))
            .expect("local should receive maker acknowledgement");

        exchange
            .process(&Event {
                ev: EXCH_SELL_TRADE_EVENT,
                exch_ts: 1,
                local_ts: 1,
                px: Decimal::from(100),
                qty: Decimal::ONE,
            })
            .expect("trade should partially fill maker order");
        local
            .process_recv_order(1, None)
            .expect("local should receive maker fill");
        assert_eq!(local.orders()[&7].filled, Decimal::ONE);

        local
            .modify(7, Decimal::from(101), PriceMatch::None, Decimal::from(3), 1)
            .expect("modify should be submitted");
        exchange
            .process_recv_order(1, None)
            .expect("exchange should process modify");
        local
            .process_recv_order(1, Some(7))
            .expect("local should receive modify result");

        let order = &local.orders()[&7];
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(order.filled, Decimal::from(3));
        assert_eq!(
            order
                .fills
                .iter()
                .map(|fill| (fill.price, fill.qty, fill.is_maker))
                .collect::<Vec<_>>(),
            vec![
                (Decimal::from(100), Decimal::ONE, true),
                (Decimal::from(101), Decimal::TWO, false),
            ]
        );
        assert_eq!(local.state_values().num_trades, 2);
    }

    #[test]
    fn cancel_preserves_partial_fills_and_ends_the_open_state() {
        let (order_e2l, order_l2e) = order_bus(ConstantLatency::new(0, 0));
        let state = || {
            State::new(
                LinearAsset::new(1.0),
                TradingValueFeeModel::new(CommonFees::new(0.0, 0.0)),
            )
        };
        let mut local = Local::new(BTreeMarketDepth::new(), state(), 0, order_l2e);
        let schedule = TickSizeSchedule::new(vec![TickSizeChange {
            effective_from: 0,
            tick_size: Decimal::ONE,
        }])
        .expect("test tick size schedule should be valid");
        let mut exchange = PartialFillExchange::new(
            BTreeMarketDepth::new(),
            state(),
            RiskAdverseQueueModel::new(),
            order_e2l,
            schedule,
        );
        local
            .submit_order(
                9,
                Side::Buy,
                Decimal::from(100),
                PriceMatch::None,
                Decimal::from(3),
                OrdType::Limit,
                TimeInForce::GTC,
                0,
            )
            .expect("maker order should be submitted");
        exchange
            .process_recv_order(0, None)
            .expect("exchange should accept maker order");
        local
            .process_recv_order(0, Some(9))
            .expect("local should receive maker acknowledgement");
        exchange
            .process(&Event {
                ev: EXCH_SELL_TRADE_EVENT,
                exch_ts: 1,
                local_ts: 1,
                px: Decimal::from(100),
                qty: Decimal::ONE,
            })
            .expect("trade should partially fill maker order");
        local
            .process_recv_order(1, None)
            .expect("local should receive partial fill");

        local.cancel(9, 1).expect("cancel should be submitted");
        exchange
            .process_recv_order(1, None)
            .expect("exchange should process cancel");
        local
            .process_recv_order(1, Some(9))
            .expect("local should receive cancel result");

        let order = &local.orders()[&9];
        assert_eq!(order.status, OrderStatus::Canceled);
        assert_eq!(order.filled, Decimal::ONE);
        assert_eq!(order.remaining(), Decimal::TWO);
        assert_eq!(order.fills.len(), 1);
        assert!(order.fills[0].is_maker);
        assert!(!order.active());
    }
}
