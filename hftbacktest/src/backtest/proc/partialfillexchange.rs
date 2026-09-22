use rust_decimal::{Decimal, prelude::ToPrimitive};
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
        order::ExchToLocal,
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
        Order, OrderId, Side, Status, TimeInForce,
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
                return self.fill::<true>(order, timestamp, true, order.price, order.leaves_qty);
            }
            Ordering::Equal => {
                // Updates the order's queue position.
                self.queue_model.trade(order, qty, &self.depth);
                let filled_qty = self.queue_model.is_filled(order, &self.depth);
                if filled_qty > Decimal::ZERO {
                    // q_ahead is negative since is_filled is true and its value represents the
                    // executable quantity of this order after execution in the queue ahead of this
                    // order.
                    let exec_qty = filled_qty.min(order.leaves_qty);
                    self.fill::<true>(order, timestamp, true, order.price, exec_qty)?;
                    if order.status == Status::Filled {
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
                return self.fill::<true>(order, timestamp, true, order.price, order.leaves_qty);
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
                    let exec_qty = filled_qty.min(order.leaves_qty);
                    self.fill::<true>(order, timestamp, true, order.price, exec_qty)?;
                    if order.status == Status::Filled {
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
        if order.status == Status::Expired
            || order.status == Status::Canceled
            || order.status == Status::Filled
        {
            return Err(BacktestError::InvalidOrderStatus);
        }

        order.maker = maker;
        if maker {
            order.exec_price = order.price;
        } else {
            order.exec_price = exec_price;
        }

        order.exec_qty = exec_qty;
        order.cum_exec_qty += exec_qty;
        order.cum_exec_value += (exec_qty * order.latest_exec_price())
            .to_f64()
            .expect("execution value should fit f64");
        if !maker {
            order.taker_price_level_count += 1;
        }
        order.leaves_qty -= exec_qty;
        if order.leaves_qty > Decimal::ZERO {
            order.status = Status::PartiallyFilled;
        } else {
            order.status = Status::Filled;
        }
        order.exch_timestamp = timestamp;

        self.state.apply_fill(order);

        if MAKE_RESPONSE {
            self.order_e2l.respond(order.clone());
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
                    self.fill::<true>(order, timestamp, true, order.price, order.leaves_qty)?;
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
                    self.fill::<true>(order, timestamp, true, order.price, order.leaves_qty)?;
                }
            }
        }
        self.remove_filled_orders();
        Ok(())
    }

    fn liquidity(&self, order: &Order) -> (Vec<(Decimal, Decimal)>, Decimal) {
        let mut levels = Vec::new();
        let mut remaining = order.leaves_qty;
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
        order.status = if order.cum_exec_qty > Decimal::ZERO {
            Status::PartiallyFilled
        } else {
            Status::New
        };
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
            order.status = Status::Expired;
            order.exch_timestamp = timestamp;
            return Ok(());
        }
        let Some(price) = resolve_price_match(order, &self.depth) else {
            order.status = Status::Expired;
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
                    order.status = Status::Expired;
                    order.exch_timestamp = timestamp;
                    return Ok(());
                }
            }
        }
        if order.order_type == OrdType::Limit && order.time_in_force == TimeInForce::GTX {
            order.status = Status::Expired;
            order.exch_timestamp = timestamp;
            return Ok(());
        }

        let (levels, remaining) = self.liquidity(order);
        if order.time_in_force == TimeInForce::FOK && remaining > Decimal::ZERO {
            order.status = Status::Expired;
            order.exch_timestamp = timestamp;
            return Ok(());
        }
        for (price, exec_qty) in levels {
            self.fill::<false>(order, timestamp, false, price, exec_qty)?;
            if order.status == Status::Filled {
                return Ok(());
            }
        }
        if order.order_type == OrdType::Limit && order.time_in_force == TimeInForce::GTC {
            let leaves_qty = order.leaves_qty;
            return self.fill::<false>(order, timestamp, false, order.price, leaves_qty);
        }
        order.status = Status::Expired;
        order.exch_timestamp = timestamp;
        Ok(())
    }
    fn ack_new(&mut self, order: &mut Order, timestamp: i64) -> Result<(), BacktestError> {
        self.ack_new_checked(order, timestamp, true)
    }

    fn ack_cancel(&mut self, order: &mut Order, timestamp: i64) -> Result<(), BacktestError> {
        let exch_order = {
            let mut order_borrowed = self.orders.borrow_mut();
            order_borrowed.remove(&order.order_id)
        };

        if exch_order.is_none() {
            order.req = Status::Rejected;
            order.exch_timestamp = timestamp;
            return Ok(());
        }

        let exch_order = exch_order.unwrap();
        let _ = std::mem::replace(order, exch_order);

        // Deletes the order.
        if order.side == Side::Buy {
            self.buy_orders
                .get_mut(&order.price)
                .unwrap()
                .remove(&order.order_id);
        } else {
            self.sell_orders
                .get_mut(&order.price)
                .unwrap()
                .remove(&order.order_id);
        }
        order.status = Status::Canceled;
        order.exch_timestamp = timestamp;
        Ok(())
    }

    fn ack_modify(&mut self, order: &mut Order, timestamp: i64) -> Result<(), BacktestError> {
        let unchanged_price = self
            .orders
            .borrow()
            .get(&order.order_id)
            .is_some_and(|existing| existing.price == order.price);
        if !unchanged_price && !price_satisfies_rule(order, timestamp, &self.tick_sizes)? {
            order.req = Status::Rejected;
            order.exch_timestamp = timestamp;
            return Ok(());
        }
        let requested_price = resolve_price_match(order, &self.depth);
        let requested_price_match = order.price_match;
        let requested_qty = order.qty;
        let request_timestamp = order.local_timestamp;

        let Some(requested_price) = requested_price else {
            order.req = Status::Rejected;
            order.exch_timestamp = timestamp;
            return Ok(());
        };

        self.ack_cancel(order, timestamp)?;
        if order.req == Status::Rejected {
            return Ok(());
        }
        order.local_timestamp = request_timestamp;
        order.price_match = requested_price_match;
        let crosses_book = order.order_type == OrdType::Limit
            && order.time_in_force == TimeInForce::GTX
            && match order.side {
                Side::Buy => self
                    .depth
                    .best_ask()
                    .is_some_and(|ask| requested_price >= ask),
                Side::Sell => self
                    .depth
                    .best_bid()
                    .is_some_and(|bid| requested_price <= bid),
            };
        if (order.cum_exec_qty > Decimal::ZERO && requested_qty <= order.cum_exec_qty)
            || crosses_book
        {
            return Ok(());
        }

        order.price = requested_price;
        order.qty = requested_qty;
        order.leaves_qty = requested_qty - order.cum_exec_qty;
        order.status = if order.cum_exec_qty > Decimal::ZERO {
            Status::PartiallyFilled
        } else {
            Status::New
        };
        self.ack_new_checked(order, timestamp, false)?;
        Ok(())
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
        while let Some(mut order) = self.order_e2l.receive(timestamp) {
            // Processes a new order.
            if order.req == Status::New {
                order.req = Status::None;
                self.ack_new(&mut order, timestamp)?;
            }
            // Processes a cancel order.
            else if order.req == Status::Canceled {
                order.req = Status::None;
                self.ack_cancel(&mut order, timestamp)?;
            }
            // Processes a modify order.
            else if order.req == Status::Replaced {
                order.req = Status::None;
                self.ack_modify(&mut order, timestamp)?;
            } else {
                return Err(BacktestError::InvalidOrderRequest);
            }
            // Makes the response.
            self.order_e2l.respond(order);
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
            order::{LocalToExch, order_bus},
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
        assert_eq!(order.status, Status::New);

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
        assert_eq!(order.status, Status::New);

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
        assert_eq!(insufficient.status, Status::Expired);
        assert_eq!(insufficient.cum_exec_qty, Decimal::ZERO);
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
        assert_eq!(sufficient.status, Status::Filled);
        assert_eq!(sufficient.cum_exec_qty, Decimal::from(3));
        assert_eq!(exchange.state.values().position, Decimal::from(3));
        assert_eq!(exchange.state.values().num_trades, 2);
    }

    #[test]
    fn multilevel_fok_ioc_and_gtc_match_both_sides() {
        for side in [Side::Buy, Side::Sell] {
            for (time_in_force, qty, status, filled, trades, levels, buy_value, sell_value) in [
                (TimeInForce::FOK, 5, Status::Expired, 0, 0, 0, 0.0, 0.0),
                (TimeInForce::FOK, 4, Status::Filled, 4, 2, 2, 402.0, 394.0),
                (TimeInForce::IOC, 5, Status::Expired, 4, 2, 2, 402.0, 394.0),
                (TimeInForce::GTC, 5, Status::Filled, 5, 3, 3, 503.0, 492.0),
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
                assert_eq!(order.cum_exec_qty, Decimal::from(filled));
                assert_eq!(order.leaves_qty, Decimal::from(qty - filled));
                assert_eq!(order.cum_exec_value, expected_value);
                assert_eq!(order.taker_price_level_count, levels);
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
        assert_eq!(buy.status, Status::Filled);
        assert_eq!(buy.cum_exec_qty, Decimal::from(3));
        assert_eq!(buy.cum_exec_value, 300.25);
        assert_eq!(buy.taker_price_level_count, 2);

        let mut sell = Order::new(
            2,
            Decimal::new(9990, 2),
            Decimal::from(3),
            Side::Sell,
            OrdType::Limit,
            TimeInForce::IOC,
        );
        exchange.ack_new(&mut sell, 10).expect("sell should match");
        assert_eq!(sell.status, Status::Filled);
        assert_eq!(sell.cum_exec_qty, Decimal::from(3));
        assert_eq!(sell.cum_exec_value, 299.75);
        assert_eq!(sell.taker_price_level_count, 2);
        assert_eq!(exchange.state.values().position, Decimal::ZERO);
        assert_eq!(exchange.state.values().num_trades, 4);
    }

    #[test]
    fn delayed_orders_use_rule_at_exchange_arrival() {
        for (local_timestamp, price, expected_status) in [
            (8, Decimal::new(10005, 2), Status::New),
            (9, Decimal::new(10005, 2), Status::Expired),
            (10, Decimal::new(10005, 2), Status::Expired),
            (9, Decimal::new(10010, 2), Status::New),
        ] {
            let (mut exchange, mut local) =
                exchange_with(switching_schedule(), ConstantLatency::new(1, 2));
            let mut order = Order::new(
                1,
                price,
                Decimal::ONE,
                Side::Buy,
                OrdType::Limit,
                TimeInForce::GTC,
            );
            order.local_timestamp = local_timestamp;
            order.req = Status::New;
            local.request(order, |_| panic!("positive latency should reach exchange"));
            let arrival = local_timestamp + 1;
            assert_eq!(exchange.earliest_recv_order_timestamp(), arrival);
            exchange
                .process_recv_order(arrival, None)
                .expect("delayed order should be processed");
            assert_eq!(local.earliest_recv_order_timestamp(), Some(arrival + 2));
            let response = local
                .receive(arrival + 2)
                .expect("exchange response should arrive after response latency");
            assert_eq!(response.status, expected_status);
            assert_eq!(response.exch_timestamp, arrival);
            assert_eq!(response.local_timestamp, local_timestamp);
            assert_eq!(
                exchange.orders.borrow().len(),
                usize::from(expected_status == Status::New)
            );
        }
    }
}
