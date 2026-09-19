use std::{
    any::Any,
    collections::HashMap,
    fmt::{Debug, Formatter},
};

use dyn_clone::DynClone;
use rust_decimal::Decimal;
use thiserror::Error;

use crate::depth::MarketDepth;

/// Indicates a buy, with specific meaning that can vary depending on the situation. For example,
/// when combined with a depth event, it means a bid-side event, while when combined with a trade
/// event, it means that the trade initiator is a buyer.
pub const BUY_EVENT: u64 = 1 << 29;

/// Indicates a sell, with specific meaning that can vary depending on the situation. For example,
/// when combined with a depth event, it means an ask-side event, while when combined with a trade
/// event, it means that the trade initiator is a seller.
pub const SELL_EVENT: u64 = 1 << 28;

/// Indicates that the market depth is changed.
pub const DEPTH_EVENT: u64 = 1;

/// Indicates that a trade occurs in the market.
pub const TRADE_EVENT: u64 = 2;

/// Indicates that the market depth is cleared.
pub const DEPTH_CLEAR_EVENT: u64 = 3;

/// Indicates that the market depth snapshot is received.
pub const DEPTH_SNAPSHOT_EVENT: u64 = 4;

/// Indicates that the best bid and best ask update event is received.
pub const DEPTH_BBO_EVENT: u64 = 5;

/// Indicates that it is a valid event to be handled by the exchange processor at the exchange
/// timestamp.
pub const EXCH_EVENT: u64 = 1 << 31;

/// Indicates that it is a valid event to be handled by the local processor at the local timestamp.
pub const LOCAL_EVENT: u64 = 1 << 30;

/// Represents a combination of [`DEPTH_CLEAR_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_DEPTH_CLEAR_EVENT: u64 = DEPTH_CLEAR_EVENT | LOCAL_EVENT;

/// Represents a combination of [`DEPTH_CLEAR_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_DEPTH_CLEAR_EVENT: u64 = DEPTH_CLEAR_EVENT | EXCH_EVENT;

/// Represents a combination of a [`DEPTH_EVENT`], [`BUY_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_BID_DEPTH_EVENT: u64 = DEPTH_EVENT | BUY_EVENT | LOCAL_EVENT;

/// Represents a combination of [`DEPTH_EVENT`], [`SELL_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_ASK_DEPTH_EVENT: u64 = DEPTH_EVENT | SELL_EVENT | LOCAL_EVENT;

/// Represents a combination of [`DEPTH_CLEAR_EVENT`], [`BUY_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_BID_DEPTH_CLEAR_EVENT: u64 = DEPTH_CLEAR_EVENT | BUY_EVENT | LOCAL_EVENT;

/// Represents a combination of [`DEPTH_CLEAR_EVENT`], [`SELL_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_ASK_DEPTH_CLEAR_EVENT: u64 = DEPTH_CLEAR_EVENT | SELL_EVENT | LOCAL_EVENT;

/// Represents a combination of [`DEPTH_SNAPSHOT_EVENT`], [`BUY_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_BID_DEPTH_SNAPSHOT_EVENT: u64 = DEPTH_SNAPSHOT_EVENT | BUY_EVENT | LOCAL_EVENT;

/// Represents a combination of [`DEPTH_SNAPSHOT_EVENT`], [`SELL_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_ASK_DEPTH_SNAPSHOT_EVENT: u64 = DEPTH_SNAPSHOT_EVENT | SELL_EVENT | LOCAL_EVENT;

/// Represents a combination of [`DEPTH_BBO_EVENT`], [`BUY_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_BID_DEPTH_BBO_EVENT: u64 = DEPTH_BBO_EVENT | BUY_EVENT | LOCAL_EVENT;

/// Represents a combination of [`DEPTH_BBO_EVENT`], [`SELL_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_ASK_DEPTH_BBO_EVENT: u64 = DEPTH_BBO_EVENT | SELL_EVENT | LOCAL_EVENT;

/// Represents a combination of [`TRADE_EVENT`], and [`LOCAL_EVENT`].
pub const LOCAL_TRADE_EVENT: u64 = TRADE_EVENT | LOCAL_EVENT;

/// Represents a combination of [`LOCAL_TRADE_EVENT`] and [`BUY_EVENT`].
pub const LOCAL_BUY_TRADE_EVENT: u64 = LOCAL_TRADE_EVENT | BUY_EVENT;

/// Represents a combination of [`LOCAL_TRADE_EVENT`] and [`SELL_EVENT`].
pub const LOCAL_SELL_TRADE_EVENT: u64 = LOCAL_TRADE_EVENT | SELL_EVENT;

/// Represents a combination of [`DEPTH_EVENT`], [`BUY_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_BID_DEPTH_EVENT: u64 = DEPTH_EVENT | BUY_EVENT | EXCH_EVENT;

/// Represents a combination of [`DEPTH_EVENT`], [`SELL_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_ASK_DEPTH_EVENT: u64 = DEPTH_EVENT | SELL_EVENT | EXCH_EVENT;

/// Represents a combination of [`DEPTH_CLEAR_EVENT`], [`BUY_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_BID_DEPTH_CLEAR_EVENT: u64 = DEPTH_CLEAR_EVENT | BUY_EVENT | EXCH_EVENT;

/// Represents a combination of [`DEPTH_CLEAR_EVENT`], [`SELL_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_ASK_DEPTH_CLEAR_EVENT: u64 = DEPTH_CLEAR_EVENT | SELL_EVENT | EXCH_EVENT;

/// Represents a combination of [`DEPTH_SNAPSHOT_EVENT`], [`BUY_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_BID_DEPTH_SNAPSHOT_EVENT: u64 = DEPTH_SNAPSHOT_EVENT | BUY_EVENT | EXCH_EVENT;

/// Represents a combination of [`DEPTH_SNAPSHOT_EVENT`], [`SELL_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_ASK_DEPTH_SNAPSHOT_EVENT: u64 = DEPTH_SNAPSHOT_EVENT | SELL_EVENT | EXCH_EVENT;

/// Represents a combination of [`DEPTH_BBO_EVENT`], [`BUY_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_BID_DEPTH_BBO_EVENT: u64 = DEPTH_BBO_EVENT | BUY_EVENT | EXCH_EVENT;

/// Represents a combination of [`DEPTH_BBO_EVENT`], [`SELL_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_ASK_DEPTH_BBO_EVENT: u64 = DEPTH_BBO_EVENT | SELL_EVENT | EXCH_EVENT;

/// Represents a combination of [`TRADE_EVENT`], and [`EXCH_EVENT`].
pub const EXCH_TRADE_EVENT: u64 = TRADE_EVENT | EXCH_EVENT;

/// Represents a combination of [`EXCH_TRADE_EVENT`] and [`BUY_EVENT`].
pub const EXCH_BUY_TRADE_EVENT: u64 = EXCH_TRADE_EVENT | BUY_EVENT;

/// Represents a combination of [`EXCH_TRADE_EVENT`] and [`SELL_EVENT`].
pub const EXCH_SELL_TRADE_EVENT: u64 = EXCH_TRADE_EVENT | SELL_EVENT;

/// Indicates that one should continue until the end of the data.
pub const UNTIL_END_OF_DATA: i64 = i64::MAX;

pub type OrderId = u64;

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum WaitOrderResponse {
    None,
    Any,
    Specified { asset_no: usize, order_id: OrderId },
}

/// Feed event data.
#[derive(Clone, PartialEq, Debug)]
pub struct Event {
    /// Event flag
    pub ev: u64,
    /// Exchange timestamp, which is the time at which the event occurs on the exchange.
    pub exch_ts: i64,
    /// Local timestamp, which is the time at which the event is received by the local.
    pub local_ts: i64,
    /// Price
    pub px: Decimal,
    /// Quantity
    pub qty: Decimal,
}

impl Event {
    /// Checks if this `Event` corresponds to the given event.
    #[inline(always)]
    pub fn is(&self, event: u64) -> bool {
        if (self.ev & event) != event {
            false
        } else {
            let event_kind = event & 0xff;
            if event_kind == 0 {
                true
            } else {
                self.ev & 0xff == event_kind
            }
        }
    }
}

/// Represents a side, which can refer to either the side of an order or the initiator's side in a
/// trade event, with the meaning varying depending on the context.
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[repr(i8)]
pub enum Side {
    /// In the market depth event, this indicates the bid side; in the market trade event, it
    /// indicates that the trade initiator is a buyer.
    Buy = 1,
    /// In the market depth event, this indicates the ask side; in the market trade event, it
    /// indicates that the trade initiator is a seller.
    Sell = -1,
}

impl AsRef<f64> for Side {
    fn as_ref(&self) -> &f64 {
        match self {
            Side::Buy => &1.0f64,
            Side::Sell => &-1.0f64,
        }
    }
}

impl AsRef<str> for Side {
    fn as_ref(&self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }
}

/// Order status
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum Status {
    None = 0,
    New = 1,
    Expired = 2,
    Filled = 3,
    Canceled = 4,
    PartiallyFilled = 5,
    Rejected = 6,
    Replaced = 7,
}

/// Time In Force
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum TimeInForce {
    /// Good 'Til Canceled
    GTC = 0,
    /// Post-only
    GTX = 1,
    /// Fill or Kill
    FOK = 2,
    /// Immediate or Cancel
    IOC = 3,
}

impl AsRef<str> for TimeInForce {
    fn as_ref(&self) -> &'static str {
        match self {
            TimeInForce::GTC => "GTC",
            TimeInForce::GTX => "GTX",
            TimeInForce::FOK => "FOK",
            TimeInForce::IOC => "IOC",
        }
    }
}

/// Exchange-side price matching mode.
///
/// The exchange resolves the requested book level when it receives a new or modify request, so
/// the resulting price reflects order entry latency.
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum PriceMatch {
    None = 0,
    Opponent = 1,
    Opponent5 = 2,
    Opponent10 = 3,
    Opponent20 = 4,
    Queue = 5,
    Queue5 = 6,
    Queue10 = 7,
    Queue20 = 8,
}

impl AsRef<str> for PriceMatch {
    fn as_ref(&self) -> &'static str {
        match self {
            PriceMatch::None => "NONE",
            PriceMatch::Opponent => "OPPONENT",
            PriceMatch::Opponent5 => "OPPONENT_5",
            PriceMatch::Opponent10 => "OPPONENT_10",
            PriceMatch::Opponent20 => "OPPONENT_20",
            PriceMatch::Queue => "QUEUE",
            PriceMatch::Queue5 => "QUEUE_5",
            PriceMatch::Queue10 => "QUEUE_10",
            PriceMatch::Queue20 => "QUEUE_20",
        }
    }
}

/// Order type
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum OrdType {
    Limit = 0,
    Market = 1,
}

impl AsRef<str> for OrdType {
    fn as_ref(&self) -> &'static str {
        match self {
            OrdType::Limit => "LIMIT",
            OrdType::Market => "MARKET",
        }
    }
}

/// Provides cloning of `Box<dyn Any>`, which is utilized in [Order] for the additional data used in
/// [`QueueModel`](`crate::backtest::models::QueueModel`).
///
/// **Usage:**
/// ```ignore
/// impl AnyClone for QueuePos {
///     fn as_any(&self) -> &dyn Any {
///         self
///     }
///
///     fn as_any_mut(&mut self) -> &mut dyn Any {
///         self
///     }
/// }
/// ```
pub trait AnyClone: DynClone {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
dyn_clone::clone_trait_object!(AnyClone);

impl AnyClone for () {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Order
#[derive(Clone)]
pub struct Order {
    /// Total order quantity, including the cumulative executed quantity.
    pub qty: Decimal,
    /// The quantity of this order that has not yet been executed. It represents the remaining
    /// quantity that is still open or active in the market after any partial fills.
    pub leaves_qty: Decimal,
    /// Executed quantity, only available when this order is executed.
    pub exec_qty: Decimal,
    /// Latest executed price, only available when this order is executed.
    pub exec_price: Decimal,
    /// Cumulative executed quantity.
    pub cum_exec_qty: Decimal,
    /// Cumulative executed value.
    pub cum_exec_value: f64,
    /// Cumulative number of price levels consumed by taker executions.
    pub taker_price_level_count: u32,
    /// Exact order price.
    pub price: Decimal,
    /// Exchange-side rule used to resolve `price` when the request arrives.
    pub price_match: PriceMatch,
    /// The time at which the exchange processes this order, ideally when the matching engine
    /// processes the order, will be set if the value is available.
    pub exch_timestamp: i64,
    /// The time at which the local receives this order or sent this order to the exchange.
    pub local_timestamp: i64,
    pub order_id: u64,
    /// Additional data used for [`QueueModel`](`crate::backtest::models::QueueModel`).
    /// This is only available in backtesting.
    pub q: Box<dyn AnyClone + Send>,
    /// Whether the order is executed as a maker, only available when this order is executed.
    pub maker: bool,
    pub order_type: OrdType,
    /// Request status:
    ///   * [`Status::New`]: Request to open a new order.
    ///   * [`Status::Canceled`]: Request to cancel an opened order.
    ///   * [`Status::Replaced`]: Request to modify an opened order.
    pub req: Status,
    pub status: Status,
    pub side: Side,
    pub time_in_force: TimeInForce,
}

impl Order {
    /// Constructs an instance of `Order`.
    pub fn new(
        order_id: u64,
        price: Decimal,
        qty: Decimal,
        side: Side,
        order_type: OrdType,
        time_in_force: TimeInForce,
    ) -> Self {
        Self {
            qty,
            leaves_qty: qty,
            price,
            price_match: PriceMatch::None,
            side,
            time_in_force,
            exch_timestamp: 0,
            status: Status::None,
            local_timestamp: 0,
            req: Status::None,
            exec_price: Decimal::ZERO,
            exec_qty: Decimal::ZERO,
            cum_exec_qty: Decimal::ZERO,
            cum_exec_value: 0.0,
            taker_price_level_count: 0,
            order_id,
            q: Box::new(()),
            maker: false,
            order_type,
        }
    }

    /// Returns the order price.
    pub fn price(&self) -> Decimal {
        self.price
    }

    /// Returns the executed price, only available when this order is executed.
    pub fn exec_price(&self) -> f64 {
        if !self.cum_exec_qty.is_zero() {
            self.cum_exec_value
                / rust_decimal::prelude::ToPrimitive::to_f64(&self.cum_exec_qty)
                    .expect("executed quantity should be representable as f64")
        } else {
            rust_decimal::prelude::ToPrimitive::to_f64(&self.latest_exec_price())
                .expect("executed price should be representable as f64")
        }
    }

    /// Returns the latest fill price.
    pub(crate) fn latest_exec_price(&self) -> Decimal {
        self.exec_price
    }

    /// Returns whether this order is cancelable.
    pub fn cancellable(&self) -> bool {
        (self.status == Status::New || self.status == Status::PartiallyFilled)
            && self.req == Status::None
    }

    /// Returns whether this order is active in the market.
    pub fn active(&self) -> bool {
        self.status == Status::New || self.status == Status::PartiallyFilled
    }

    /// Returns whether this order has an ongoing request.
    pub fn pending(&self) -> bool {
        self.req != Status::None
    }

    /// Updates this order with the response produced by a backtesting processor.
    pub fn update(&mut self, order: &Order) {
        //assert!(order.exch_timestamp >= self.exch_timestamp);
        if order.exch_timestamp < self.exch_timestamp {
            println!(
                "Warning: Perhaps an inaccurate order response update occurs: an order previously \
                updated by a later exchange timestamp is updated by an earlier one. \
                This issue is primarily caused by incorrect or inconsistent timestamp ordering \
                across the files.\n \
                order={:?}, \
                response={:?}",
                self, order
            );
        }

        self.qty = order.qty;
        self.leaves_qty = order.leaves_qty;
        self.price = order.price;
        self.price_match = order.price_match;
        self.side = order.side;
        self.time_in_force = order.time_in_force;

        if order.exch_timestamp > 0 {
            self.exch_timestamp = order.exch_timestamp;
        }
        self.status = order.status;
        // if order.local_timestamp > 0 {
        //     self.local_timestamp = order.local_timestamp;
        // }
        self.req = order.req;
        self.exec_price = order.exec_price;
        self.exec_qty = order.exec_qty;
        self.cum_exec_qty = order.cum_exec_qty;
        self.cum_exec_value = order.cum_exec_value;
        self.taker_price_level_count = order.taker_price_level_count;
        self.order_id = order.order_id;
        self.q = order.q.clone();
        self.maker = order.maker;
        self.order_type = order.order_type;
    }
}

impl Debug for Order {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Order")
            .field("qty", &self.qty)
            .field("leaves_qty", &self.leaves_qty)
            .field("price", &self.price)
            .field("price_match", &self.price_match)
            .field("side", &self.side)
            .field("time_in_force", &self.time_in_force)
            .field("exch_timestamp", &self.exch_timestamp)
            .field("status", &self.status)
            .field("local_timestamp", &self.local_timestamp)
            .field("req", &self.req)
            .field("exec_price", &self.exec_price)
            .field("exec_qty", &self.exec_qty)
            .field("cum_exec_qty", &self.cum_exec_qty)
            .field("cum_exec_value", &self.cum_exec_value)
            .field("taker_price_level_count", &self.taker_price_level_count)
            .field("order_id", &self.order_id)
            .field("maker", &self.maker)
            .field("order_type", &self.order_type)
            .finish()
    }
}

/// Provides state values.
#[derive(PartialEq, Clone, Debug, Default)]
pub struct StateValues {
    pub position: Decimal,
    pub balance: f64,
    pub fee: f64,
    // todo: currently, they are cumulative values, but they need to be values within the record
    //       interval.
    pub num_trades: i64,
    pub trading_volume: f64,
    pub trading_value: f64,
}

/// Provides errors that can occur in builders.
#[derive(Error, Debug)]
pub enum BuildError {
    #[error("`{0}` is required")]
    BuilderIncomplete(&'static str),
    #[error("{0}")]
    InvalidArgument(&'static str),
    #[error("{0:?}")]
    Error(#[from] anyhow::Error),
}

/// Describes an order to submit to the backtester.
pub struct OrderRequest {
    pub order_id: u64,
    pub price: Decimal,
    /// If not [`PriceMatch::None`], the exchange ignores `price` and resolves this mode on arrival.
    pub price_match: PriceMatch,
    pub qty: Decimal,
    pub side: Side,
    pub time_in_force: TimeInForce,
    pub order_type: OrdType,
}

/// Provides a bot interface for backtesting.
pub trait Bot<MD>
where
    MD: MarketDepth,
{
    type Error;

    /// Returns the current timestamp within the replayed data.
    fn current_timestamp(&self) -> i64;

    /// Returns the number of assets.
    fn num_assets(&self) -> usize;

    /// Returns the position you currently hold.
    ///
    /// * `asset_no` - Asset number from which the position will be retrieved.
    fn position(&self, asset_no: usize) -> Decimal;

    /// Returns the state's values such as balance, fee, and so on.
    fn state_values(&self, asset_no: usize) -> &StateValues;

    /// Returns the [`MarketDepth`].
    ///
    /// * `asset_no` - Asset number from which the market depth will be retrieved.
    fn depth(&self, asset_no: usize) -> &MD;

    /// Returns the last market trades.
    ///
    /// * `asset_no` - Asset number from which the last market trades will be retrieved.
    fn last_trades(&self, asset_no: usize) -> &[Event];

    /// Clears the last market trades from the buffer.
    ///
    /// * `asset_no` - Asset number at which this command will be executed. If `None`, all last
    ///   trades in any assets will be cleared.
    fn clear_last_trades(&mut self, asset_no: Option<usize>);

    /// Returns a hash map of order IDs and their corresponding [`Order`]s.
    ///
    /// * `asset_no` - Asset number from which orders will be retrieved.
    fn orders(&self, asset_no: usize) -> &HashMap<OrderId, Order>;

    /// Places a buy order.
    ///
    /// * `asset_no` - Asset number at which this command will be executed.
    /// * `order_id` - The unique order ID; there should not be any existing order with the same ID
    ///   on both local and exchange sides.
    /// * `price` - Order price.
    /// * `qty` - Quantity to buy.
    /// * `time_in_force` - Available [`TimeInForce`] options vary depending on the exchange model.
    ///   See to the exchange model for details.
    ///
    /// * `order_type` - Available [`OrdType`] options vary depending on the exchange model. See to
    ///   the exchange model for details.
    ///
    /// * `wait` - If true, wait until the order placement response is received.
    #[allow(clippy::too_many_arguments)]
    fn submit_buy_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: Decimal,
        qty: Decimal,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error>;

    /// Places a buy order whose price is resolved from exchange depth on arrival.
    #[allow(clippy::too_many_arguments)]
    fn submit_buy_order_with_price_match(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        qty: Decimal,
        time_in_force: TimeInForce,
        order_type: OrdType,
        price_match: PriceMatch,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error>;

    /// Places a sell order.
    ///
    /// * `asset_no` - Asset number at which this command will be executed.
    /// * `order_id` - The unique order ID; there should not be any existing order with the same ID
    ///   on both local and exchange sides.
    /// * `price` - Order price.
    /// * `qty` - Quantity to buy.
    /// * `time_in_force` - Available [`TimeInForce`] options vary depending on the exchange model.
    ///   See to the exchange model for details.
    ///
    /// * `order_type` - Available [`OrdType`] options vary depending on the exchange model. See to
    ///   the exchange model for details.
    ///
    /// * `wait` - If true, wait until the order placement response is received.
    #[allow(clippy::too_many_arguments)]
    fn submit_sell_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: Decimal,
        qty: Decimal,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error>;

    /// Places a sell order whose price is resolved from exchange depth on arrival.
    #[allow(clippy::too_many_arguments)]
    fn submit_sell_order_with_price_match(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        qty: Decimal,
        time_in_force: TimeInForce,
        order_type: OrdType,
        price_match: PriceMatch,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error>;

    /// Places an order.
    fn submit_order(
        &mut self,
        asset_no: usize,
        order: OrderRequest,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error>;

    /// Modifies an open order.
    ///
    /// * `asset_no` - Asset number at which this command will be executed.
    /// * `order_id` - Order ID to modify.
    /// * `price` - Order price.
    /// * `qty` - New total order quantity for L2 backtesting, including the cumulative executed
    ///   quantity.
    /// * `wait` - If true, wait until the order modification response is received.
    fn modify(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: Decimal,
        qty: Decimal,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error>;

    /// Modifies an open order using a price resolved from exchange depth on arrival.
    fn modify_with_price_match(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        qty: Decimal,
        price_match: PriceMatch,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error>;

    /// Cancels an open order.
    ///
    /// * `asset_no` - Asset number at which this command will be executed.
    /// * `order_id` - Order ID to cancel.
    /// * `wait` - If true, wait until the order placement response is received.
    fn cancel(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error>;

    /// Clears inactive orders from the local orders whose status is neither [`Status::New`] nor
    /// [`Status::PartiallyFilled`].
    fn clear_inactive_orders(&mut self, asset_no: Option<usize>);

    /// Waits for the response of the order with the given order ID until timeout.
    fn wait_order_response(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        timeout: i64,
    ) -> Result<ElapseResult, Self::Error>;

    /// Wait until the next feed is received, or until timeout.
    fn wait_next_feed(
        &mut self,
        include_order_resp: bool,
        timeout: i64,
    ) -> Result<ElapseResult, Self::Error>;

    /// Elapses the specified duration.
    ///
    /// Args:
    /// * `duration` - Duration to elapse. Nanoseconds is the default unit. However, unit should be
    ///   the same as the data's timestamp unit.
    ///
    /// Returns:
    ///   `Ok(true)` if the method reaches the specified timestamp within the data. If the end of
    ///   the data is reached before the specified timestamp, it returns `Ok(false)`.
    fn elapse(&mut self, duration: i64) -> Result<ElapseResult, Self::Error>;

    /// Elapses simulated processing time.
    ///
    /// The [elapse()](Self::elapse()) method exclusively manages replay time, so this method can be
    /// used to account for strategy computation time explicitly.
    ///
    /// Args:
    /// * `duration` - Duration to elapse. Nanoseconds is the default unit. However, unit should be
    ///   the same as the data's timestamp unit.
    ///
    /// Returns:
    ///   `Ok(true)` if the method reaches the specified timestamp within the data. If the end of
    ///   the data is reached before the specified timestamp, it returns `Ok(false)`.
    fn elapse_bt(&mut self, duration: i64) -> Result<ElapseResult, Self::Error>;

    /// Closes this backtester.
    fn close(&mut self) -> Result<(), Self::Error>;

    /// Returns the last feed's exchange timestamp and local receipt timestamp.
    fn feed_latency(&self, asset_no: usize) -> Option<(i64, i64)>;

    /// Returns the last order's request timestamp, exchange timestamp, and response receipt
    /// timestamp.
    fn order_latency(&self, asset_no: usize) -> Option<(i64, i64, i64)>;
}

/// Provides statistics and [`StateValues`] recording features for backtesting result analysis.
pub trait Recorder {
    type Error;

    /// Records the current [`StateValues`].
    fn record<MD, I>(&mut self, hbt: &I) -> Result<(), Self::Error>
    where
        I: Bot<MD>,
        MD: MarketDepth;
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
pub enum ElapseResult {
    Ok,
    EndOfData,
    MarketFeed,
    OrderResponse,
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use crate::types::{BUY_EVENT, Event, LOCAL_BID_DEPTH_CLEAR_EVENT};

    #[test]
    fn event_flags_are_independent_of_decimal_payloads() {
        let event = Event {
            ev: LOCAL_BID_DEPTH_CLEAR_EVENT | BUY_EVENT,
            exch_ts: 0,
            local_ts: 0,
            px: Decimal::new(10025, 2),
            qty: Decimal::new(1, 8),
        };
        assert!(event.is(LOCAL_BID_DEPTH_CLEAR_EVENT));
        assert!(event.is(BUY_EVENT));
    }
}
