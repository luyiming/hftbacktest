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

/// The exchange model without partial fills.
///
/// Support order types: [OrdType::Limit](crate::types::OrdType::Limit)
/// Support time-in-force: [`TimeInForce::GTC`], [`TimeInForce::GTX`]
///
/// **Conditions for Full Execution**
///
/// Buy order in the order book
///
/// - Your order price >= the best ask price
/// - Your order price > sell trade price
/// - Your order is at the front of the queue and your order price == sell trade price
///
/// Sell order in the order book
///
/// - Your order price <= the best bid price
/// - Your order price < buy trade price
/// - Your order is at the front of the queue && your order price == buy trade price
///
/// **Liquidity-Taking Order**
///
/// Regardless of the quantity at the best, liquidity-taking orders will be fully executed at the
/// best. Be aware that this may cause unrealistic fill simulations if you attempt to execute a
/// large quantity.
///
pub struct NoPartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: QueueModel<MD>,
    MD: MarketDepth,
    FM: FeeModel,
{
    // key: order_id, value: Order<Q>
    orders: Rc<RefCell<HashMap<OrderId, Order>>>,
    // key: order's price, value: order_ids
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

impl<AT, LM, QM, MD, FM> NoPartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: QueueModel<MD>,
    MD: MarketDepth,
    FM: FeeModel,
{
    /// Constructs an instance of `NoPartialFillExchange`.
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
                return self.fill::<true>(order, timestamp, true, order.price);
            }
            Ordering::Equal => {
                // Updates the order's queue position.
                self.queue_model.trade(order, qty, &self.depth);
                if self.queue_model.is_filled(order, &self.depth) > Decimal::ZERO {
                    self.filled_orders.push(order.order_id);
                    return self.fill::<true>(order, timestamp, true, order.price);
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
                return self.fill::<true>(order, timestamp, true, order.price);
            }
            Ordering::Less => {}
            Ordering::Equal => {
                // Updates the order's queue position.
                self.queue_model.trade(order, qty, &self.depth);
                if self.queue_model.is_filled(order, &self.depth) > Decimal::ZERO {
                    self.filled_orders.push(order.order_id);
                    return self.fill::<true>(order, timestamp, true, order.price);
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
    ) -> Result<(), BacktestError> {
        if order.status.is_terminal() {
            return Err(BacktestError::InvalidOrderStatus);
        }

        let fill = OrderFill {
            price: if maker { order.price } else { exec_price },
            qty: order.remaining(),
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
                    self.fill::<true>(order, timestamp, true, order.price)?;
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
                    self.fill::<true>(order, timestamp, true, order.price)?;
                }
            }
        }
        self.remove_filled_orders();
        Ok(())
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

        if order.side == Side::Buy {
            match order.order_type {
                OrdType::Limit => {
                    // Checks if the buy order price is greater than or equal to the current best ask.
                    if self.depth.best_ask().is_some_and(|ask| order.price >= ask) {
                        match order.time_in_force {
                            TimeInForce::GTX => {
                                order.status = OrderStatus::Expired;
                                order.exch_timestamp = timestamp;
                                Ok(())
                            }
                            TimeInForce::GTC | TimeInForce::FOK | TimeInForce::IOC => {
                                // Since this always fills the full quantity, both FOK and IOC
                                // orders are also fully filled at the best price.
                                // Takes the market.
                                self.fill::<false>(
                                    order,
                                    timestamp,
                                    false,
                                    self.depth.best_ask().expect("crossing ask should exist"),
                                )
                            }
                        }
                    } else {
                        match order.time_in_force {
                            TimeInForce::GTC | TimeInForce::GTX => {
                                // Initializes the order's queue position.
                                self.queue_model.new_order(order, &self.depth);
                                order.status = OrderStatus::Open;
                                // The exchange accepts this order.
                                self.buy_orders
                                    .entry(order.price)
                                    .or_default()
                                    .insert(order.order_id);

                                order.exch_timestamp = timestamp;
                                self.orders
                                    .borrow_mut()
                                    .insert(order.order_id, order.clone());
                                Ok(())
                            }
                            TimeInForce::FOK | TimeInForce::IOC => {
                                order.status = OrderStatus::Expired;
                                order.exch_timestamp = timestamp;
                                Ok(())
                            }
                        }
                    }
                }
                OrdType::Market => {
                    // Takes the market.
                    if let Some(ask) = self.depth.best_ask() {
                        self.fill::<false>(order, timestamp, false, ask)
                    } else {
                        order.status = OrderStatus::Expired;
                        order.exch_timestamp = timestamp;
                        Ok(())
                    }
                }
            }
        } else {
            match order.order_type {
                OrdType::Limit => {
                    // Checks if the sell order price is less than or equal to the current best bid.
                    if self.depth.best_bid().is_some_and(|bid| order.price <= bid) {
                        match order.time_in_force {
                            TimeInForce::GTX => {
                                order.status = OrderStatus::Expired;
                                order.exch_timestamp = timestamp;
                                Ok(())
                            }
                            TimeInForce::GTC | TimeInForce::FOK | TimeInForce::IOC => {
                                // Since this always fills the full quantity, both FOK and IOC
                                // orders are also fully filled at the best price.
                                // Takes the market.
                                self.fill::<false>(
                                    order,
                                    timestamp,
                                    false,
                                    self.depth.best_bid().expect("crossing bid should exist"),
                                )
                            }
                        }
                    } else {
                        match order.time_in_force {
                            TimeInForce::GTC | TimeInForce::GTX => {
                                // Initializes the order's queue position.
                                self.queue_model.new_order(order, &self.depth);
                                order.status = OrderStatus::Open;
                                // The exchange accepts this order.
                                self.sell_orders
                                    .entry(order.price)
                                    .or_default()
                                    .insert(order.order_id);

                                order.exch_timestamp = timestamp;
                                self.orders
                                    .borrow_mut()
                                    .insert(order.order_id, order.clone());
                                Ok(())
                            }
                            TimeInForce::FOK | TimeInForce::IOC => {
                                order.status = OrderStatus::Expired;
                                order.exch_timestamp = timestamp;
                                Ok(())
                            }
                        }
                    }
                }
                OrdType::Market => {
                    // Takes the market.
                    if let Some(bid) = self.depth.best_bid() {
                        self.fill::<false>(order, timestamp, false, bid)
                    } else {
                        order.status = OrderStatus::Expired;
                        order.exch_timestamp = timestamp;
                        Ok(())
                    }
                }
            }
        }
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

impl<AT, LM, QM, MD, FM> Processor for NoPartialFillExchange<AT, LM, QM, MD, FM>
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
            .ok_or(SnapshotError::Unsupported("NoPartialFillExchange"))?;
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
            order_bus::order_bus,
            rules::{TickSizeChange, TickSizeSchedule},
        },
        depth::{BTreeMarketDepth, L2MarketDepth},
        types::{OrdType, PriceMatch},
    };

    fn exchange() -> NoPartialFillExchange<
        LinearAsset,
        ConstantLatency,
        RiskAdverseQueueModel<BTreeMarketDepth>,
        BTreeMarketDepth,
        TradingValueFeeModel<CommonFees>,
    > {
        let mut depth = BTreeMarketDepth::new();
        depth.update_bid_depth(Decimal::from(99), Decimal::ONE, 0);
        depth.update_ask_depth(Decimal::from(101), Decimal::ONE, 0);
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
        let (order_e2l, _) = order_bus(ConstantLatency::new(0, 0));
        NoPartialFillExchange::new(
            depth,
            State::new(
                LinearAsset::new(1.0),
                TradingValueFeeModel::new(CommonFees::new(0.0, 0.0)),
            ),
            RiskAdverseQueueModel::new(),
            order_e2l,
            schedule,
        )
    }

    #[test]
    fn rule_switch_allows_unchanged_price_and_rejects_changed_invalid_price() {
        let mut exchange = exchange();
        let price = Decimal::new(10005, 2);
        let mut order = Order::new(
            1,
            price,
            Decimal::ONE,
            Side::Buy,
            OrdType::Limit,
            TimeInForce::GTC,
        );
        exchange.ack_new(&mut order, 0).unwrap();
        assert_eq!(order.status, OrderStatus::Open);

        let quantity_only = exchange
            .ack_modify(1, price, PriceMatch::None, Decimal::TWO, 10)
            .expect("quantity-only modify should be processed")
            .0
            .expect("quantity-only modify should be accepted");
        assert_eq!(quantity_only.status, OrderStatus::Open);
        assert_eq!(exchange.orders.borrow()[&1].price, price);

        let mut invalid_price = quantity_only.clone();
        invalid_price.price = Decimal::new(10006, 2);
        invalid_price.price_match = PriceMatch::None;
        assert!(
            !exchange
                .ack_modify(
                    1,
                    invalid_price.price,
                    invalid_price.price_match,
                    invalid_price.qty,
                    10,
                )
                .expect("invalid price modify should be rejected normally")
                .2
        );
        assert_eq!(exchange.orders.borrow()[&1].price, price);

        let mut missing_price_match = quantity_only;
        missing_price_match.price_match = PriceMatch::Opponent20;
        assert!(
            !exchange
                .ack_modify(
                    1,
                    missing_price_match.price,
                    missing_price_match.price_match,
                    missing_price_match.qty,
                    10,
                )
                .expect("missing price match should be rejected normally")
                .2
        );
        assert_eq!(exchange.orders.borrow()[&1].price, price);
    }

    #[test]
    fn first_ask_fills_crossing_buy_order() {
        let mut exchange = exchange();
        exchange.depth.clear_depth(Side::Sell, None);
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
        exchange.depth.clear_depth(Side::Buy, None);
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
}
