use rust_decimal::Decimal;
use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use crate::{
    backtest::{
        BacktestError,
        assettype::AssetType,
        models::{FeeModel, LatencyModel, QueueModel},
        order_bus::ExchToLocal,
        proc::{
            Processor,
            price_match::{price_satisfies_rule, resolve_price_match},
        },
        rules::TickSizeSchedule,
        snapshot::{ProcessorSnapshotFn, SnapshotContext, SnapshotError, SnapshotState},
        state::State,
    },
    depth::{L2MarketDepth, MarketDepth},
    prelude::OrdType,
    types::{
        EXCH_ASK_DEPTH_CLEAR_EVENT, EXCH_ASK_DEPTH_EVENT, EXCH_ASK_DEPTH_SNAPSHOT_EVENT,
        EXCH_BID_DEPTH_CLEAR_EVENT, EXCH_BID_DEPTH_EVENT, EXCH_BID_DEPTH_SNAPSHOT_EVENT,
        EXCH_BUY_TRADE_EVENT, EXCH_DEPTH_CLEAR_EVENT, EXCH_EVENT, EXCH_SELL_TRADE_EVENT, Event,
        Order, OrderFill, OrderId, OrderRequest, OrderStatus, OrderUpdate, RequestOutcome,
        RequestResult, Side, TimeInForce,
    },
};

/// The exchange model with partial fills.
///
/// * Support order types: [OrdType::Limit](crate::types::OrdType::Limit)
/// * Support time-in-force: [`TimeInForce::GTC`], [`TimeInForce::FOK`], [`TimeInForce::IOC`],
///   [`TimeInForce::GTX`]
///
/// **Conditions for Full Execution**
/// Buy order in the order book
///
/// - Your order price >= the best ask price
/// - Your order price > sell trade price
///
/// Sell order in the order book
///
/// - Your order price <= the best bid price
/// - Your order price < buy trade price
///
/// **Conditions for Partial Execution**
/// Buy order in the order book
///
/// - Filled by (remaining) sell trade quantity: your order is at the front of the queue && your
///   order price == sell trade price
///
/// Sell order in the order book
///
/// - Filled by (remaining) buy trade quantity: your order is at the front of the queue && your
///   order price == buy trade price
///
/// **Liquidity-Taking Order**
/// Liquidity-taking orders will be executed based on the quantity of the order book, even though
/// the best price and quantity do not change due to your execution. Be aware that this may cause
/// unrealistic fill simulations if you attempt to execute a large quantity.
///
/// **General Comment**
/// Simulating partial fills accurately can be challenging, as they may indicate potential market
/// impact. The rule of thumb is to ensure that your backtesting results align with your live
/// results.
/// (more comment will be added...)
///
pub struct PartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: QueueModel<MD>,
    MD: MarketDepth,
    FM: FeeModel,
{
    // key: order_id, value: Order
    orders: Rc<RefCell<HashMap<OrderId, Order>>>,
    // key: order's price tick, value: order_ids
    buy_orders: HashMap<Decimal, HashSet<OrderId>>,
    sell_orders: HashMap<Decimal, HashSet<OrderId>>,

    order_e2l: ExchToLocal<LM>,

    depth: MD,
    state: State<AT, FM>,
    queue_model: QM,

    filled_orders: Vec<OrderId>,
    snapshot_fn: Option<ProcessorSnapshotFn<Self>>,
    tick_sizes: TickSizeSchedule,
}

impl<AT, LM, QM, MD, FM> PartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: QueueModel<MD>,
    MD: MarketDepth,
    FM: FeeModel,
{
    /// Constructs an instance of `PartialFillExchange`.
    pub fn new(
        depth: MD,
        state: State<AT, FM>,
        queue_model: QM,
        order_e2l: ExchToLocal<LM>,
        tick_sizes: TickSizeSchedule,
    ) -> Self {
        Self {
            orders: Default::default(),
            buy_orders: Default::default(),
            sell_orders: Default::default(),
            order_e2l,
            depth,
            state,
            queue_model,
            filled_orders: Default::default(),
            snapshot_fn: None,
            tick_sizes,
        }
    }

    pub(crate) fn enable_snapshot(mut self) -> Self
    where
        AT: SnapshotState + 'static,
        LM: SnapshotState + 'static,
        QM: SnapshotState + 'static,
        MD: L2MarketDepth + SnapshotState + 'static,
        FM: SnapshotState + 'static,
    {
        self.snapshot_fn = Some(|source, context| {
            Box::new(Self {
                orders: Rc::new(RefCell::new(source.orders.borrow().clone())),
                buy_orders: source.buy_orders.clone(),
                sell_orders: source.sell_orders.clone(),
                order_e2l: source.order_e2l.snapshot(context),
                depth: source.depth.clone(),
                state: source.state.clone(),
                queue_model: source.queue_model.clone(),
                filled_orders: source.filled_orders.clone(),
                snapshot_fn: source.snapshot_fn,
                tick_sizes: source.tick_sizes.clone(),
            })
        });
        self
    }

    fn check_if_sell_filled(
        &mut self,
        order: &mut Order,
        price: Decimal,
        qty: Decimal,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        match order.price.cmp(&price) {
            Ordering::Greater => {}
            Ordering::Less => {
                self.filled_orders.push(order.order_id);
                return self.fill::<true>(order, timestamp, true, order.price, order.remaining());
            }
            Ordering::Equal => {
                // Updates the order's queue position.
                self.queue_model.trade(order, qty, &self.depth);
                let filled_qty = self.queue_model.is_filled(order, &self.depth);
                if filled_qty > Decimal::ZERO {
                    // q_ahead is negative since is_filled is true and its value represents the
                    // executable quantity of this order after execution in the queue ahead of this
                    // order.
                    let exec_qty = filled_qty.min(order.remaining());
                    self.fill::<true>(order, timestamp, true, order.price, exec_qty)?;
                    if order.status == OrderStatus::Filled {
                        self.filled_orders.push(order.order_id);
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn check_if_buy_filled(
        &mut self,
        order: &mut Order,
        price: Decimal,
        qty: Decimal,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        match order.price.cmp(&price) {
            Ordering::Greater => {
                self.filled_orders.push(order.order_id);
                return self.fill::<true>(order, timestamp, true, order.price, order.remaining());
            }
            Ordering::Less => {}
            Ordering::Equal => {
                // Updates the order's queue position.
                self.queue_model.trade(order, qty, &self.depth);
                let filled_qty = self.queue_model.is_filled(order, &self.depth);
                if filled_qty > Decimal::ZERO {
                    // q_ahead is negative since is_filled is true and its value represents the
                    // executable quantity of this order after execution in the queue ahead of this
                    // order.
                    let exec_qty = filled_qty.min(order.remaining());
                    self.fill::<true>(order, timestamp, true, order.price, exec_qty)?;
                    if order.status == OrderStatus::Filled {
                        self.filled_orders.push(order.order_id);
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn fill<const MAKE_RESPONSE: bool>(
        &mut self,
        order: &mut Order,
        timestamp: i64,
        maker: bool,
        exec_price: Decimal,
        exec_qty: Decimal,
    ) -> Result<(), BacktestError> {
        if order.status.is_terminal() {
            return Err(BacktestError::InvalidOrderStatus);
        }

        let fill = OrderFill {
            price: if maker { order.price } else { exec_price },
            qty: exec_qty,
            exch_timestamp: timestamp,
            is_maker: maker,
        };
        order.apply_fill(fill.clone());
        self.state.apply_fill(order.side, &fill);

        if MAKE_RESPONSE {
            self.order_e2l.respond(OrderUpdate {
                order_id: order.order_id,
                exch_timestamp: timestamp,
                fills: vec![fill],
                state: Some(order.state()),
                request_result: None,
            });
        }
        Ok(())
    }

    fn remove_filled_orders(&mut self) {
        if !self.filled_orders.is_empty() {
            let mut orders = self.orders.borrow_mut();
            for order_id in self.filled_orders.drain(..) {
                let order = orders.remove(&order_id).unwrap();
                if order.side == Side::Buy {
                    self.buy_orders
                        .get_mut(&order.price)
                        .unwrap()
                        .remove(&order_id);
                } else {
                    self.sell_orders
                        .get_mut(&order.price)
                        .unwrap()
                        .remove(&order_id);
                }
            }
        }
    }

    fn on_bid_qty_chg(&mut self, price: Decimal, prev_qty: Decimal, new_qty: Decimal) {
        let orders = self.orders.clone();
        if let Some(order_ids) = self.buy_orders.get(&price) {
            for order_id in order_ids.iter() {
                let mut orders_borrowed = orders.borrow_mut();
                let order = orders_borrowed.get_mut(order_id).unwrap();
                self.queue_model
                    .depth(order, prev_qty, new_qty, &self.depth);
            }
        }
    }

    fn on_ask_qty_chg(&mut self, price: Decimal, prev_qty: Decimal, new_qty: Decimal) {
        let orders = self.orders.clone();
        if let Some(order_ids) = self.sell_orders.get(&price) {
            for order_id in order_ids.iter() {
                let mut orders_borrowed = orders.borrow_mut();
                let order = orders_borrowed.get_mut(order_id).unwrap();
                self.queue_model
                    .depth(order, prev_qty, new_qty, &self.depth);
            }
        }
    }

    fn on_best_bid_update(
        &mut self,
        new_best_price: Decimal,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        {
            let orders = self.orders.clone();
            let mut orders_borrowed = orders.borrow_mut();
            for order in orders_borrowed.values_mut() {
                if order.side == Side::Sell && order.price <= new_best_price {
                    self.filled_orders.push(order.order_id);
                    self.fill::<true>(order, timestamp, true, order.price, order.remaining())?;
                }
            }
        }
        self.remove_filled_orders();
        Ok(())
    }

    fn on_best_ask_update(
        &mut self,
        new_best_price: Decimal,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        {
            let orders = self.orders.clone();
            let mut orders_borrowed = orders.borrow_mut();
            for order in orders_borrowed.values_mut() {
                if order.side == Side::Buy && order.price >= new_best_price {
                    self.filled_orders.push(order.order_id);
                    self.fill::<true>(order, timestamp, true, order.price, order.remaining())?;
                }
            }
        }
        self.remove_filled_orders();
        Ok(())
    }

    fn liquidity(&self, order: &Order) -> (Vec<(Decimal, Decimal)>, Decimal) {
        let mut levels = Vec::new();
        let mut remaining = order.remaining();
        let limit = (order.order_type == OrdType::Limit).then_some(order.price);
        let mut visit = |price: Decimal, qty: Decimal| {
            if limit.is_some_and(|limit| match order.side {
                Side::Buy => price > limit,
                Side::Sell => price < limit,
            }) {
                return false;
            }
            if qty > Decimal::ZERO {
                let exec_qty = qty.min(remaining);
                if exec_qty > Decimal::ZERO {
                    levels.push((price, exec_qty));
                    remaining -= exec_qty;
                }
            }
            remaining > Decimal::ZERO
        };
        match order.side {
            Side::Buy => {
                if let Some(start) = self.depth.best_ask() {
                    self.depth.for_each_ask_depth_from(start, &mut visit);
                }
            }
            Side::Sell => {
                if let Some(start) = self.depth.best_bid() {
                    self.depth.for_each_bid_depth_from(start, &mut visit);
                }
            }
        }
        (levels, remaining)
    }

    fn rest_order(&mut self, order: &mut Order, timestamp: i64) {
        self.queue_model.new_order(order, &self.depth);
        order.status = OrderStatus::Open;
        let orders_at_price = match order.side {
            Side::Buy => self.buy_orders.entry(order.price).or_default(),
            Side::Sell => self.sell_orders.entry(order.price).or_default(),
        };
        orders_at_price.insert(order.order_id);
        order.exch_timestamp = timestamp;
        self.orders
            .borrow_mut()
            .insert(order.order_id, order.clone());
    }

    fn ack_new_checked(
        &mut self,
        order: &mut Order,
        timestamp: i64,
        validate_rule: bool,
    ) -> Result<(), BacktestError> {
        if self.orders.borrow().contains_key(&order.order_id) {
            return Err(BacktestError::OrderIdExist);
        }
        if validate_rule && !price_satisfies_rule(order, timestamp, &self.tick_sizes)? {
            order.status = OrderStatus::Expired;
            order.exch_timestamp = timestamp;
            return Ok(());
        }
        let Some(price) = resolve_price_match(order, &self.depth) else {
            order.status = OrderStatus::Expired;
            order.exch_timestamp = timestamp;
            return Ok(());
        };
        order.price = price;
        let crosses = match order.side {
            Side::Buy => self.depth.best_ask().is_some_and(|ask| order.price >= ask),
            Side::Sell => self.depth.best_bid().is_some_and(|bid| order.price <= bid),
        };
        if order.order_type == OrdType::Limit && !crosses {
            match order.time_in_force {
                TimeInForce::GTC | TimeInForce::GTX => {
                    self.rest_order(order, timestamp);
                    return Ok(());
                }
                TimeInForce::FOK | TimeInForce::IOC => {
                    order.status = OrderStatus::Expired;
                    order.exch_timestamp = timestamp;
                    return Ok(());
                }
            }
        }
        if order.order_type == OrdType::Limit && order.time_in_force == TimeInForce::GTX {
            order.status = OrderStatus::Expired;
            order.exch_timestamp = timestamp;
            return Ok(());
        }

        let (levels, remaining) = self.liquidity(order);
        if order.time_in_force == TimeInForce::FOK && remaining > Decimal::ZERO {
            order.status = OrderStatus::Expired;
            order.exch_timestamp = timestamp;
            return Ok(());
        }
        for (price, exec_qty) in levels {
            self.fill::<false>(order, timestamp, false, price, exec_qty)?;
            if order.status == OrderStatus::Filled {
                return Ok(());
            }
        }
        if order.order_type == OrdType::Limit && order.time_in_force == TimeInForce::GTC {
            let remaining = order.remaining();
            return self.fill::<false>(order, timestamp, false, order.price, remaining);
        }
        order.status = OrderStatus::Expired;
        order.exch_timestamp = timestamp;
        Ok(())
    }
    fn ack_new(&mut self, order: &mut Order, timestamp: i64) -> Result<(), BacktestError> {
        self.ack_new_checked(order, timestamp, true)
    }

    fn ack_cancel(&mut self, order_id: OrderId, timestamp: i64) -> Option<Order> {
        let mut order = self.orders.borrow_mut().remove(&order_id)?;
        if order.side == Side::Buy {
            self.buy_orders
                .get_mut(&order.price)
                .expect("buy price level should contain open order")
                .remove(&order.order_id);
        } else {
            self.sell_orders
                .get_mut(&order.price)
                .expect("sell price level should contain open order")
                .remove(&order.order_id);
        }
        order.status = OrderStatus::Canceled;
        order.exch_timestamp = timestamp;
        Some(order)
    }

    fn ack_modify(
        &mut self,
        order_id: OrderId,
        price: Decimal,
        price_match: crate::types::PriceMatch,
        qty: Decimal,
        timestamp: i64,
    ) -> Result<(Option<Order>, Vec<OrderFill>, bool), BacktestError> {
        let Some(existing) = self.orders.borrow().get(&order_id).cloned() else {
            return Ok((None, Vec::new(), false));
        };
        let previous_fill_count = existing.fills.len();
        let mut candidate = existing.clone();
        let unchanged_price = candidate.price == price;
        candidate.price = price;
        candidate.price_match = price_match;
        candidate.qty = qty;
        if !unchanged_price && !price_satisfies_rule(&candidate, timestamp, &self.tick_sizes)? {
            return Ok((Some(existing), Vec::new(), false));
        }
        let Some(requested_price) = resolve_price_match(&candidate, &self.depth) else {
            return Ok((Some(existing), Vec::new(), false));
        };
        let crosses_book = candidate.order_type == OrdType::Limit
            && candidate.time_in_force == TimeInForce::GTX
            && match candidate.side {
                Side::Buy => self
                    .depth
                    .best_ask()
                    .is_some_and(|ask| requested_price >= ask),
                Side::Sell => self
                    .depth
                    .best_bid()
                    .is_some_and(|bid| requested_price <= bid),
            };
        let mut order = self
            .ack_cancel(order_id, timestamp)
            .expect("validated modify target should remain open");
        if (order.filled > Decimal::ZERO && qty <= order.filled) || crosses_book {
            return Ok((Some(order), Vec::new(), true));
        }

        order.price = requested_price;
        order.price_match = price_match;
        order.qty = qty;
        order.status = OrderStatus::Open;
        self.ack_new_checked(&mut order, timestamp, false)?;
        let fills = order.fills[previous_fill_count..].to_vec();
        Ok((Some(order), fills, true))
    }
}

impl<AT, LM, QM, MD, FM> Processor for PartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: QueueModel<MD>,
    MD: MarketDepth + L2MarketDepth,
    FM: FeeModel,
{
    fn snapshot_processor(
        &self,
        context: &mut SnapshotContext,
    ) -> Result<Box<dyn Processor>, SnapshotError> {
        let snapshot = self
            .snapshot_fn
            .ok_or(SnapshotError::Unsupported("PartialFillExchange"))?;
        Ok(snapshot(self, context))
    }

    fn event_seen_timestamp(&self, event: &Event) -> Option<i64> {
        event.is(EXCH_EVENT).then_some(event.exch_ts)
    }

    fn process(&mut self, event: &Event) -> Result<(), BacktestError> {
        if event.is(EXCH_BID_DEPTH_CLEAR_EVENT) {
            self.depth.clear_depth(Side::Buy, Some(event.px));
        } else if event.is(EXCH_ASK_DEPTH_CLEAR_EVENT) {
            self.depth.clear_depth(Side::Sell, Some(event.px));
        } else if event.is(EXCH_DEPTH_CLEAR_EVENT) {
            self.depth.clear_depth(Side::Buy, None);
            self.depth.clear_depth(Side::Sell, None);
        } else if event.is(EXCH_BID_DEPTH_EVENT) || event.is(EXCH_BID_DEPTH_SNAPSHOT_EVENT) {
            let update = self
                .depth
                .update_bid_depth(event.px, event.qty, event.exch_ts);
            self.on_bid_qty_chg(update.level_price, update.previous_qty, update.new_qty);
            if let Some(best_bid) = update.best_price
                && update
                    .previous_best_price
                    .is_none_or(|previous| best_bid > previous)
            {
                self.on_best_bid_update(best_bid, update.timestamp)?;
            }
            if event.is(EXCH_BID_DEPTH_SNAPSHOT_EVENT) {
                self.depth.mark_depth_ready();
            }
        } else if event.is(EXCH_ASK_DEPTH_EVENT) || event.is(EXCH_ASK_DEPTH_SNAPSHOT_EVENT) {
            let update = self
                .depth
                .update_ask_depth(event.px, event.qty, event.exch_ts);
            self.on_ask_qty_chg(update.level_price, update.previous_qty, update.new_qty);
            if let Some(best_ask) = update.best_price
                && update
                    .previous_best_price
                    .is_none_or(|previous| best_ask < previous)
            {
                self.on_best_ask_update(best_ask, update.timestamp)?;
            }
            if event.is(EXCH_ASK_DEPTH_SNAPSHOT_EVENT) {
                self.depth.mark_depth_ready();
            }
        } else if event.is(EXCH_BUY_TRADE_EVENT) {
            let price = event.px;
            let qty = event.qty;
            {
                let orders = self.orders.clone();
                let mut orders_borrowed = orders.borrow_mut();
                for order in orders_borrowed.values_mut() {
                    if order.side == Side::Sell {
                        self.check_if_sell_filled(order, price, qty, event.exch_ts)?;
                    }
                }
            }
            self.remove_filled_orders();
        } else if event.is(EXCH_SELL_TRADE_EVENT) {
            let price = event.px;
            let qty = event.qty;
            {
                let orders = self.orders.clone();
                let mut orders_borrowed = orders.borrow_mut();
                for order in orders_borrowed.values_mut() {
                    if order.side == Side::Buy {
                        self.check_if_buy_filled(order, price, qty, event.exch_ts)?;
                    }
                }
            }
            self.remove_filled_orders();
        }

        Ok(())
    }

    fn process_recv_order(
        &mut self,
        timestamp: i64,
        _wait_resp_order_id: Option<OrderId>,
    ) -> Result<bool, BacktestError> {
        while let Some(request) = self.order_e2l.receive(timestamp) {
            let request_id = request.request_id();
            let local_timestamp = request.local_timestamp();
            let order_id = request.order_id();
            let kind = request.kind();
            let (order, fills, accepted) = match request {
                OrderRequest::New { order: new, .. } => {
                    let mut order = Order::new(
                        new.order_id,
                        new.price,
                        new.qty,
                        new.side,
                        new.order_type,
                        new.time_in_force,
                    );
                    order.price_match = new.price_match;
                    self.ack_new(&mut order, timestamp)?;
                    let fills = order.fills.clone();
                    (Some(order), fills, true)
                }
                OrderRequest::Cancel { order_id, .. } => {
                    let order = self.ack_cancel(order_id, timestamp);
                    let accepted = order.is_some();
                    (order, Vec::new(), accepted)
                }
                OrderRequest::Modify {
                    order_id,
                    price,
                    price_match,
                    qty,
                    ..
                } => {
                    let (order, fills, accepted) =
                        self.ack_modify(order_id, price, price_match, qty, timestamp)?;
                    (order, fills, accepted)
                }
            };
            self.order_e2l.respond(OrderUpdate {
                order_id,
                exch_timestamp: timestamp,
                fills,
                state: order.as_ref().map(Order::state),
                request_result: Some(RequestResult {
                    request_id,
                    local_timestamp,
                    kind,
                    outcome: if accepted {
                        RequestOutcome::Accepted
                    } else {
                        RequestOutcome::Rejected
                    },
                }),
            });
        }
        Ok(false)
    }

    fn earliest_recv_order_timestamp(&self) -> i64 {
        self.order_e2l
            .earliest_recv_order_timestamp()
            .unwrap_or(i64::MAX)
    }

    fn earliest_send_order_timestamp(&self) -> i64 {
        self.order_e2l
            .earliest_send_order_timestamp()
            .unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::*;
    use crate::{
        backtest::{
            assettype::LinearAsset,
            models::{CommonFees, ConstantLatency, RiskAdverseQueueModel, TradingValueFeeModel},
            order_bus::{LocalToExch, order_bus},
            rules::{TickSizeChange, TickSizeSchedule},
        },
        depth::BTreeMarketDepth,
    };

    type TestExchange = PartialFillExchange<
        LinearAsset,
        ConstantLatency,
        RiskAdverseQueueModel<BTreeMarketDepth>,
        BTreeMarketDepth,
        TradingValueFeeModel<CommonFees>,
    >;

    fn exchange_with(
        schedule: TickSizeSchedule,
        latency: ConstantLatency,
    ) -> (TestExchange, LocalToExch<ConstantLatency>) {
        let (order_e2l, order_l2e) = order_bus(latency);
        let exchange = PartialFillExchange::new(
            BTreeMarketDepth::new(),
            State::new(
                LinearAsset::new(1.0),
                TradingValueFeeModel::new(CommonFees::new(0.0, 0.0)),
            ),
            RiskAdverseQueueModel::new(),
            order_e2l,
            schedule,
        );
        (exchange, order_l2e)
    }

    fn exchange() -> TestExchange {
        let schedule = TickSizeSchedule::new(vec![TickSizeChange {
            effective_from: 0,
            tick_size: Decimal::ONE,
        }])
        .expect("test tick size schedule should be valid");
        exchange_with(schedule, ConstantLatency::new(0, 0)).0
    }

    fn switching_schedule() -> TickSizeSchedule {
        TickSizeSchedule::new(vec![
            TickSizeChange {
                effective_from: 0,
                tick_size: Decimal::new(1, 2),
            },
            TickSizeChange {
                effective_from: 10,
                tick_size: Decimal::new(1, 1),
            },
        ])
        .expect("test tick size schedule should be valid")
    }

    #[test]
    fn first_ask_fills_crossing_buy_order() {
        let mut exchange = exchange();
        let mut order = Order::new(
            1,
            Decimal::from(100),
            Decimal::ONE,
            Side::Buy,
            OrdType::Limit,
            TimeInForce::GTC,
        );
        exchange.ack_new(&mut order, 0).unwrap();
        assert_eq!(order.status, OrderStatus::Open);

        exchange
            .process(&Event {
                ev: EXCH_ASK_DEPTH_EVENT,
                exch_ts: 1,
                local_ts: 2,
                px: Decimal::from(99),
                qty: Decimal::ONE,
            })
            .unwrap();

        assert!(exchange.orders.borrow().is_empty());
        assert_eq!(exchange.state.values().position, Decimal::ONE);
    }

    #[test]
    fn first_bid_fills_crossing_sell_order() {
        let mut exchange = exchange();
        let mut order = Order::new(
            1,
            Decimal::from(99),
            Decimal::ONE,
            Side::Sell,
            OrdType::Limit,
            TimeInForce::GTC,
        );
        exchange.ack_new(&mut order, 0).unwrap();
        assert_eq!(order.status, OrderStatus::Open);

        exchange
            .process(&Event {
                ev: EXCH_BID_DEPTH_EVENT,
                exch_ts: 1,
                local_ts: 2,
                px: Decimal::from(100),
                qty: Decimal::ONE,
            })
            .unwrap();

        assert!(exchange.orders.borrow().is_empty());
        assert_eq!(exchange.state.values().position, -Decimal::ONE);
    }

    #[test]
    fn liquidity_plans_only_the_levels_needed_to_fill() {
        let mut exchange = exchange();
        for price in [100, 101, 102] {
            exchange
                .depth
                .update_ask_depth(Decimal::from(price), Decimal::TWO, 0);
        }

        let market_order = Order::new(
            1,
            Decimal::ZERO,
            Decimal::ONE,
            Side::Buy,
            OrdType::Market,
            TimeInForce::IOC,
        );
        assert_eq!(
            exchange.liquidity(&market_order),
            (vec![(Decimal::from(100), Decimal::ONE)], Decimal::ZERO)
        );

        let limit_order = Order::new(
            2,
            Decimal::from(100),
            Decimal::from(3),
            Side::Buy,
            OrdType::Limit,
            TimeInForce::FOK,
        );
        assert_eq!(
            exchange.liquidity(&limit_order),
            (vec![(Decimal::from(100), Decimal::TWO)], Decimal::ONE)
        );

        for price in [98, 99] {
            exchange
                .depth
                .update_bid_depth(Decimal::from(price), Decimal::TWO, 0);
        }
        let sell_order = Order::new(
            3,
            Decimal::from(98),
            Decimal::from(3),
            Side::Sell,
            OrdType::Limit,
            TimeInForce::FOK,
        );
        assert_eq!(
            exchange.liquidity(&sell_order),
            (
                vec![
                    (Decimal::from(99), Decimal::TWO),
                    (Decimal::from(98), Decimal::ONE),
                ],
                Decimal::ZERO,
            )
        );
    }

    #[test]
    fn fok_checks_available_liquidity_before_filling() {
        let mut exchange = exchange();
        for price in [100, 101] {
            exchange
                .depth
                .update_ask_depth(Decimal::from(price), Decimal::TWO, 0);
        }

        let mut insufficient = Order::new(
            1,
            Decimal::from(100),
            Decimal::from(3),
            Side::Buy,
            OrdType::Limit,
            TimeInForce::FOK,
        );
        exchange
            .ack_new(&mut insufficient, 0)
            .expect("insufficient FOK order should be processed");
        assert_eq!(insufficient.status, OrderStatus::Expired);
        assert_eq!(insufficient.filled, Decimal::ZERO);
        assert_eq!(exchange.state.values().position, Decimal::ZERO);

        let mut sufficient = Order::new(
            2,
            Decimal::from(101),
            Decimal::from(3),
            Side::Buy,
            OrdType::Limit,
            TimeInForce::FOK,
        );
        exchange
            .ack_new(&mut sufficient, 0)
            .expect("sufficient FOK order should be processed");
        assert_eq!(sufficient.status, OrderStatus::Filled);
        assert_eq!(sufficient.filled, Decimal::from(3));
        assert_eq!(exchange.state.values().position, Decimal::from(3));
        assert_eq!(exchange.state.values().num_trades, 2);
    }

    #[test]
    fn multilevel_fok_ioc_and_gtc_match_both_sides() {
        for side in [Side::Buy, Side::Sell] {
            for (time_in_force, qty, status, filled, trades, levels, buy_value, sell_value) in [
                (TimeInForce::FOK, 5, OrderStatus::Expired, 0, 0, 0, 0.0, 0.0),
                (
                    TimeInForce::FOK,
                    4,
                    OrderStatus::Filled,
                    4,
                    2,
                    2,
                    402.0,
                    394.0,
                ),
                (
                    TimeInForce::IOC,
                    5,
                    OrderStatus::Expired,
                    4,
                    2,
                    2,
                    402.0,
                    394.0,
                ),
                (
                    TimeInForce::GTC,
                    5,
                    OrderStatus::Filled,
                    5,
                    3,
                    3,
                    503.0,
                    492.0,
                ),
            ] {
                let mut exchange = exchange();
                for (price, level_qty) in [(100, 2), (101, 2)] {
                    exchange.depth.update_ask_depth(
                        Decimal::from(price),
                        Decimal::from(level_qty),
                        0,
                    );
                }
                for (price, level_qty) in [(99, 2), (98, 2)] {
                    exchange.depth.update_bid_depth(
                        Decimal::from(price),
                        Decimal::from(level_qty),
                        0,
                    );
                }
                let limit = if side == Side::Buy { 101 } else { 98 };
                let mut order = Order::new(
                    1,
                    Decimal::from(limit),
                    Decimal::from(qty),
                    side,
                    OrdType::Limit,
                    time_in_force,
                );
                exchange
                    .ack_new(&mut order, 1)
                    .expect("multilevel order should be processed");

                let expected_value = if side == Side::Buy {
                    buy_value
                } else {
                    sell_value
                };
                assert_eq!(order.status, status, "{side:?} {time_in_force:?}");
                assert_eq!(order.filled, Decimal::from(filled));
                assert_eq!(order.remaining(), Decimal::from(qty - filled));
                assert_eq!(
                    order.filled_value(),
                    Decimal::from_f64_retain(expected_value)
                        .expect("expected value should be an exact decimal")
                );
                assert_eq!(order.taker_fill_count(), levels);
                assert_eq!(exchange.state.values().num_trades, trades);
                assert_eq!(
                    exchange.state.values().position,
                    Decimal::from(if side == Side::Buy { filled } else { -filled })
                );
                assert!(exchange.orders.borrow().is_empty());
            }
        }
    }

    #[test]
    fn both_sides_match_old_book_levels_after_tick_size_switch() {
        let (mut exchange, _) = exchange_with(switching_schedule(), ConstantLatency::new(0, 0));
        for (price, qty) in [(10005, 1), (10010, 2)] {
            exchange
                .depth
                .update_ask_depth(Decimal::new(price, 2), Decimal::from(qty), 9);
        }
        for (price, qty) in [(9995, 1), (9990, 2)] {
            exchange
                .depth
                .update_bid_depth(Decimal::new(price, 2), Decimal::from(qty), 9);
        }

        let mut buy = Order::new(
            1,
            Decimal::new(10010, 2),
            Decimal::from(3),
            Side::Buy,
            OrdType::Limit,
            TimeInForce::FOK,
        );
        exchange.ack_new(&mut buy, 10).expect("buy should match");
        assert_eq!(buy.status, OrderStatus::Filled);
        assert_eq!(buy.filled, Decimal::from(3));
        assert_eq!(buy.filled_value(), Decimal::new(30025, 2));
        assert_eq!(buy.taker_fill_count(), 2);

        let mut sell = Order::new(
            2,
            Decimal::new(9990, 2),
            Decimal::from(3),
            Side::Sell,
            OrdType::Limit,
            TimeInForce::IOC,
        );
        exchange.ack_new(&mut sell, 10).expect("sell should match");
        assert_eq!(sell.status, OrderStatus::Filled);
        assert_eq!(sell.filled, Decimal::from(3));
        assert_eq!(sell.filled_value(), Decimal::new(29975, 2));
        assert_eq!(sell.taker_fill_count(), 2);
        assert_eq!(exchange.state.values().position, Decimal::ZERO);
        assert_eq!(exchange.state.values().num_trades, 4);
    }

    #[test]
    fn delayed_orders_use_rule_at_exchange_arrival() {
        for (local_timestamp, price, expected_status) in [
            (8, Decimal::new(10005, 2), OrderStatus::Open),
            (9, Decimal::new(10005, 2), OrderStatus::Expired),
            (10, Decimal::new(10005, 2), OrderStatus::Expired),
            (9, Decimal::new(10010, 2), OrderStatus::Open),
        ] {
            let (mut exchange, mut local) =
                exchange_with(switching_schedule(), ConstantLatency::new(1, 2));
            local.request(OrderRequest::New {
                request_id: 0,
                local_timestamp,
                order: crate::types::NewOrder {
                    order_id: 1,
                    price,
                    price_match: crate::types::PriceMatch::None,
                    qty: Decimal::ONE,
                    side: Side::Buy,
                    order_type: OrdType::Limit,
                    time_in_force: TimeInForce::GTC,
                },
            });
            let arrival = local_timestamp + 1;
            assert_eq!(exchange.earliest_recv_order_timestamp(), arrival);
            exchange
                .process_recv_order(arrival, None)
                .expect("delayed order should be processed");
            assert_eq!(local.earliest_recv_order_timestamp(), Some(arrival + 2));
            let response = local
                .receive(arrival + 2)
                .expect("exchange response should arrive after response latency");
            assert_eq!(
                response
                    .state
                    .expect("accepted request should include order state")
                    .status,
                expected_status
            );
            assert_eq!(response.exch_timestamp, arrival);
            assert_eq!(
                exchange.orders.borrow().len(),
                usize::from(expected_status == OrderStatus::Open)
            );
        }
    }
}
