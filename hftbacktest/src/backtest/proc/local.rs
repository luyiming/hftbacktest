use rust_decimal::Decimal;
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

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
    trades: VecDeque<Event>,
    record_trades: bool,
    trade_horizon: Option<Duration>,
    trade_time: i64,
    trades_since: Option<i64>,
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
            trades: VecDeque::with_capacity(last_trades_cap),
            record_trades: last_trades_cap > 0,
            trade_horizon: None,
            trade_time: 0,
            trades_since: None,
            last_feed_latency: None,
            last_order_latency: None,
            snapshot_fn: None,
        }
    }

    pub(crate) fn configure_trades(mut self, enabled: bool, horizon: Option<Duration>) -> Self {
        self.record_trades = enabled;
        self.trade_horizon = horizon;
        self
    }

    pub(crate) fn enable_snapshot(mut self) -> Self
    where
        AT: SnapshotState + 'static,
        LM: SnapshotState + 'static,
        MD: L2MarketDepth + SnapshotState + 'static,
        FM: SnapshotState + 'static,
    {
        self.snapshot_fn = Some(|source, context| {
            // Preserve allocated storage for subsequent batches, including an empty buffer.
            let mut trades = VecDeque::with_capacity(source.trades.capacity());
            trades.extend(source.trades.iter().cloned());
            Box::new(Self {
                orders: source.orders.clone(),
                pending_requests: source.pending_requests.clone(),
                last_request_results: source.last_request_results.clone(),
                next_request_id: source.next_request_id,
                order_l2e: source.order_l2e.snapshot(context),
                depth: source.depth.clone(),
                state: source.state.clone(),
                trades,
                record_trades: source.record_trades,
                trade_horizon: source.trade_horizon,
                trade_time: source.trade_time,
                trades_since: source.trades_since,
                last_feed_latency: source.last_feed_latency,
                last_order_latency: source.last_order_latency,
                snapshot_fn: source.snapshot_fn,
            })
        });
        self
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

    fn last_trades(&self) -> std::collections::vec_deque::Iter<'_, Event> {
        self.trades.iter()
    }

    fn clear_last_trades(&mut self) {
        self.trades.clear();
        self.trades_since = self.trades_since.map(|_| self.trade_time);
    }

    fn last_trades_since(&self) -> Option<i64> {
        self.trades_since
    }

    fn advance_trade_time(&mut self, timestamp: i64) {
        self.trade_time = timestamp;
        if !self.record_trades {
            return;
        }
        let since = self.trades_since.get_or_insert(timestamp);
        if let Some(horizon) = self.trade_horizon {
            let cutoff = (i128::from(timestamp) - horizon.as_nanos() as i128)
                .max(i128::from(i64::MIN)) as i64;
            *since = (*since).max(cutoff);
            while self
                .trades
                .front()
                .is_some_and(|event| event.local_ts <= cutoff)
            {
                self.trades.pop_front();
            }
        }
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
        self.advance_trade_time(ev.local_ts);
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
        else if ev.is(LOCAL_TRADE_EVENT) && self.record_trades {
            self.trades.push_back(ev.clone());
            self.advance_trade_time(ev.local_ts);
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
            }
            if let Some(result) = update.request_result {
                self.last_request_results.insert(update.order_id, result);
                self.pending_requests.remove(&update.order_id);
            }
        }
        Ok(wait_resp_order_received)
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

    fn trade_recording_local(
        capacity: usize,
    ) -> Local<LinearAsset, ConstantLatency, BTreeMarketDepth, TradingValueFeeModel<CommonFees>>
    {
        let (_, order_l2e) = order_bus(ConstantLatency::new(0, 0));
        Local::new(
            BTreeMarketDepth::new(),
            State::new(
                LinearAsset::new(Decimal::ONE),
                TradingValueFeeModel::new(CommonFees::new(Decimal::ZERO, Decimal::ZERO)),
            ),
            capacity,
            order_l2e,
        )
        .enable_snapshot()
    }

    fn market_trade(timestamp: i64) -> Event {
        Event {
            ev: crate::types::LOCAL_BUY_TRADE_EVENT,
            exch_ts: timestamp,
            local_ts: timestamp,
            px: Decimal::from(100),
            qty: Decimal::ONE,
        }
    }

    #[test]
    fn records_all_trades_past_initial_capacity_until_cleared() {
        let mut local = trade_recording_local(1);
        let trades: Vec<_> = (1..=10).map(market_trade).collect();
        for trade in &trades {
            local.process(trade).expect("trade should be processed");
        }
        assert_eq!(
            local.last_trades().collect::<Vec<_>>(),
            trades.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            local.last_trades().collect::<Vec<_>>(),
            trades.iter().collect::<Vec<_>>()
        );
        let capacity = local.trades.capacity();
        local.clear_last_trades();
        assert!(local.last_trades().len() == 0);
        assert_eq!(local.trades.capacity(), capacity);
        local
            .process(&trades[0])
            .expect("trade should be processed");
        assert_eq!(
            local.last_trades().collect::<Vec<_>>(),
            trades[..1].iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn trade_snapshots_preserve_pending_batches_and_recording_after_clear() {
        let mut local = trade_recording_local(1);
        let trades: Vec<_> = (1..=10).map(market_trade).collect();
        for trade in &trades {
            local.process(trade).expect("trade should be processed");
        }
        let mut branch = local
            .snapshot_local(&mut SnapshotContext::default())
            .expect("local snapshot should succeed");
        local.clear_last_trades();
        assert_eq!(
            branch.last_trades().collect::<Vec<_>>(),
            trades.iter().collect::<Vec<_>>()
        );
        branch.clear_last_trades();
        let mut empty_branch = branch
            .snapshot_local(&mut SnapshotContext::default())
            .expect("empty local snapshot should succeed");
        for trade in &trades {
            empty_branch
                .process(trade)
                .expect("trade should be processed");
        }
        assert_eq!(
            empty_branch.last_trades().collect::<Vec<_>>(),
            trades.iter().collect::<Vec<_>>()
        );
        assert!(branch.last_trades().len() == 0);
        assert!(local.last_trades().len() == 0);
    }

    #[test]
    fn zero_capacity_disables_recording_including_after_snapshot() {
        let mut local = trade_recording_local(0);
        local
            .process(&market_trade(1))
            .expect("trade should be processed");
        assert!(local.last_trades().len() == 0);
        local.clear_last_trades();
        let mut branch = local
            .snapshot_local(&mut SnapshotContext::default())
            .expect("local snapshot should succeed");
        branch
            .process(&market_trade(2))
            .expect("trade should be processed");
        assert!(branch.last_trades().len() == 0);
    }

    #[test]
    fn preserves_each_fill_in_a_multilevel_order_update() {
        let (order_e2l, order_l2e) = order_bus(ConstantLatency::new(0, 0));
        let state = || {
            State::new(
                LinearAsset::new(Decimal::ONE),
                TradingValueFeeModel::new(CommonFees::new(Decimal::ZERO, Decimal::ZERO)),
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
                LinearAsset::new(Decimal::ONE),
                TradingValueFeeModel::new(CommonFees::new(Decimal::ZERO, Decimal::ZERO)),
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
                LinearAsset::new(Decimal::ONE),
                TradingValueFeeModel::new(CommonFees::new(Decimal::ZERO, Decimal::ZERO)),
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
