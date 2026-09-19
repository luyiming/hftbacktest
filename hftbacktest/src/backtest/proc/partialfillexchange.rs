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
        _prev_best_price: Option<Decimal>,
        new_best_price: Option<Decimal>,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        {
            let orders = self.orders.clone();
            let mut orders_borrowed = orders.borrow_mut();
            if let Some(new_best_price) = new_best_price {
                for order in orders_borrowed.values_mut() {
                    if order.side == Side::Sell && order.price <= new_best_price {
                        self.filled_orders.push(order.order_id);
                        self.fill::<true>(order, timestamp, true, order.price, order.leaves_qty)?;
                    }
                }
            }
        }
        self.remove_filled_orders();
        Ok(())
    }

    fn on_best_ask_update(
        &mut self,
        _prev_best_price: Option<Decimal>,
        new_best_price: Option<Decimal>,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        {
            let orders = self.orders.clone();
            let mut orders_borrowed = orders.borrow_mut();
            if let Some(new_best_price) = new_best_price {
                for order in orders_borrowed.values_mut() {
                    if order.side == Side::Buy && order.price >= new_best_price {
                        self.filled_orders.push(order.order_id);
                        self.fill::<true>(order, timestamp, true, order.price, order.leaves_qty)?;
                    }
                }
            }
        }
        self.remove_filled_orders();
        Ok(())
    }

    fn liquidity(&self, side: Side, limit: Option<Decimal>) -> Vec<(Decimal, Decimal)> {
        let mut levels = Vec::new();
        match side {
            Side::Buy => {
                if let Some(start) = self.depth.best_ask() {
                    self.depth.for_each_ask_depth_from(start, |price, qty| {
                        if limit.is_some_and(|limit| price > limit) {
                            return false;
                        }
                        if qty > Decimal::ZERO {
                            levels.push((price, qty));
                        }
                        true
                    });
                }
            }
            Side::Sell => {
                if let Some(start) = self.depth.best_bid() {
                    self.depth.for_each_bid_depth_from(start, |price, qty| {
                        if limit.is_some_and(|limit| price < limit) {
                            return false;
                        }
                        if qty > Decimal::ZERO {
                            levels.push((price, qty));
                        }
                        true
                    });
                }
            }
        }
        levels
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

        let limit = (order.order_type == OrdType::Limit).then_some(order.price);
        let levels = self.liquidity(order.side, limit);
        if order.time_in_force == TimeInForce::FOK {
            let available: Decimal = levels.iter().map(|(_, qty)| *qty).sum();
            if available < order.leaves_qty {
                order.status = Status::Expired;
                order.exch_timestamp = timestamp;
                return Ok(());
            }
        }
        for (price, qty) in levels {
            let exec_qty = qty.min(order.leaves_qty);
            if exec_qty > Decimal::ZERO {
                self.fill::<false>(order, timestamp, false, price, exec_qty)?;
            }
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
            let (price, prev_best_bid, best_bid, prev_qty, new_qty, timestamp) = self
                .depth
                .update_bid_depth(event.px, event.qty, event.exch_ts);
            self.on_bid_qty_chg(price, prev_qty, new_qty);
            if best_bid > prev_best_bid {
                self.on_best_bid_update(prev_best_bid, best_bid, timestamp)?;
            }
            if event.is(EXCH_BID_DEPTH_SNAPSHOT_EVENT) {
                self.depth.mark_depth_ready();
            }
        } else if event.is(EXCH_ASK_DEPTH_EVENT) || event.is(EXCH_ASK_DEPTH_SNAPSHOT_EVENT) {
            let (price, prev_best_ask, best_ask, prev_qty, new_qty, timestamp) = self
                .depth
                .update_ask_depth(event.px, event.qty, event.exch_ts);
            self.on_ask_qty_chg(price, prev_qty, new_qty);
            if best_ask < prev_best_ask {
                self.on_best_ask_update(prev_best_ask, best_ask, timestamp)?;
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
