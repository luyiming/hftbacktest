use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use crate::{
    backtest::{models::LatencyModel, snapshot::SnapshotContext},
    types::Order,
};

/// Provides a bus for transporting backtesting orders between the exchange and the local model
/// based on the given timestamp.
#[derive(Clone, Debug, Default)]
pub struct OrderBus {
    // i64 timestamp represents the time when the order is **expected** to be received by the other side.
    order_list: Rc<RefCell<VecDeque<(Order, i64)>>>,
}

impl OrderBus {
    /// Copies pending messages once per branch, preserving its internal bus connections.
    pub fn snapshot(&self, context: &mut SnapshotContext) -> Self {
        let key = Rc::as_ptr(&self.order_list) as usize;
        context
            .buses
            .entry(key)
            .or_insert_with(|| Self {
                order_list: Rc::new(RefCell::new(self.order_list.borrow().clone())),
            })
            .clone()
    }

    /// Constructs an instance of ``OrderBus``.
    pub fn new() -> Self {
        Default::default()
    }

    /// Returns the timestamp of the earliest order in the bus.
    pub fn earliest_timestamp(&self) -> Option<i64> {
        self.order_list.borrow().front().map(|(_order, ts)| *ts)
    }

    /// Appends the order to the bus with the timestamp.
    ///
    /// To prevent the timestamp of the order from becoming disordered, it enforces that the given
    /// timestamp must be equal to or greater than the latest timestamp in the bus.
    ///
    /// In crypto exchanges that use REST APIs, it may be still possible for order requests sent
    /// later to reach the matching engine before order requests sent earlier. However, for the
    /// purpose of simplifying the backtesting process, all requests and responses are assumed to be
    /// in order.
    pub fn append(&mut self, order: Order, timestamp: i64) {
        let mut order_list = self.order_list.borrow_mut();
        let latest_timestamp = order_list.back().map_or(0, |(_, timestamp)| *timestamp);
        let timestamp = timestamp.max(latest_timestamp);
        order_list.push_back((order, timestamp));
    }

    /// Resets this to clear it.
    pub fn reset(&mut self) {
        self.order_list.borrow_mut().clear();
    }

    /// Returns the number of orders in the bus.
    pub fn len(&self) -> usize {
        self.order_list.borrow().len()
    }

    /// Returns ``true`` if the ``OrderBus`` is empty.
    pub fn is_empty(&self) -> bool {
        self.order_list.borrow().is_empty()
    }

    /// Removes the first order and its timestamp and returns it, or ``None`` if the bus is empty.
    pub fn pop_front(&mut self) -> Option<(Order, i64)> {
        self.order_list.borrow_mut().pop_front()
    }
}

/// Provides a bidirectional order bus connecting the exchange to the local.
pub struct ExchToLocal<LM> {
    to_exch: OrderBus,
    to_local: OrderBus,
    order_latency: LM,
}

impl<LM> ExchToLocal<LM>
where
    LM: LatencyModel,
{
    pub(crate) fn snapshot(&self, context: &mut SnapshotContext) -> Self
    where
        LM: crate::backtest::snapshot::SnapshotState,
    {
        Self {
            to_exch: self.to_exch.snapshot(context),
            to_local: self.to_local.snapshot(context),
            order_latency: self.order_latency.clone(),
        }
    }

    /// Returns the timestamp of the earliest order to be received by the exchange from the local.
    pub fn earliest_recv_order_timestamp(&self) -> Option<i64> {
        self.to_exch.earliest_timestamp()
    }

    /// Returns the timestamp of the earliest order sent from the exchange to the local.
    pub fn earliest_send_order_timestamp(&self) -> Option<i64> {
        self.to_local.earliest_timestamp()
    }

    /// Responds to the local with the order processed by the exchange.
    pub fn respond(&mut self, order: Order) {
        let local_recv_timestamp =
            order.exch_timestamp + self.order_latency.response(order.exch_timestamp, &order);
        self.to_local.append(order, local_recv_timestamp);
    }

    /// Receives the order request from the local, which is expected to be received at
    /// `receipt_timestamp`.
    pub fn receive(&mut self, receipt_timestamp: i64) -> Option<Order> {
        if let Some(timestamp) = self.to_exch.earliest_timestamp() {
            if timestamp == receipt_timestamp {
                self.to_exch.pop_front().map(|(order, _)| order)
            } else {
                assert!(timestamp > receipt_timestamp);
                None
            }
        } else {
            None
        }
    }
}

/// Provides a bidirectional order bus connecting the local to the exchange.
pub struct LocalToExch<LM> {
    to_exch: OrderBus,
    to_local: OrderBus,
    order_latency: LM,
}

impl<LM> LocalToExch<LM>
where
    LM: LatencyModel,
{
    pub(crate) fn snapshot(&self, context: &mut SnapshotContext) -> Self
    where
        LM: crate::backtest::snapshot::SnapshotState,
    {
        Self {
            to_exch: self.to_exch.snapshot(context),
            to_local: self.to_local.snapshot(context),
            order_latency: self.order_latency.clone(),
        }
    }

    /// Returns the timestamp of the earliest order to be received by the local from the exchange.
    pub fn earliest_recv_order_timestamp(&self) -> Option<i64> {
        self.to_local.earliest_timestamp()
    }

    /// Returns the timestamp of the earliest order sent from the local to the exchange.
    pub fn earliest_send_order_timestamp(&self) -> Option<i64> {
        self.to_exch.earliest_timestamp()
    }

    /// Sends the order request to the exchange.
    /// If it is rejected before reaching the matching engine (as reflected in the order latency
    /// information), `reject` is invoked and the rejection response is appended to the local order
    /// bus.
    pub fn request<F>(&mut self, mut order: Order, mut reject: F)
    where
        F: FnMut(&mut Order),
    {
        let order_entry_latency = self.order_latency.entry(order.local_timestamp, &order);
        // Negative latency indicates that the order is rejected for technical reasons, and its
        // value represents the latency that the local experiences when receiving the rejection
        // notification.
        if order_entry_latency < 0 {
            // Rejects the order.
            reject(&mut order);
            let rej_recv_timestamp = order.local_timestamp - order_entry_latency;
            self.to_local.append(order, rej_recv_timestamp);
        } else {
            let exch_recv_timestamp = order.local_timestamp + order_entry_latency;
            self.to_exch.append(order, exch_recv_timestamp);
        }
    }

    /// Receives the order response from the exchange, which is expected to be received at
    /// `receipt_timestamp`.
    pub fn receive(&mut self, receipt_timestamp: i64) -> Option<Order> {
        if let Some(timestamp) = self.to_local.earliest_timestamp() {
            if timestamp == receipt_timestamp {
                self.to_local.pop_front().map(|(order, _)| order)
            } else {
                assert!(timestamp > receipt_timestamp);
                None
            }
        } else {
            None
        }
    }
}

/// Creates bidirectional order buses with the order latency model.
pub fn order_bus<LM>(order_latency: LM) -> (ExchToLocal<LM>, LocalToExch<LM>)
where
    LM: LatencyModel + Clone,
{
    let to_exch = OrderBus::new();
    let to_local = OrderBus::new();
    (
        ExchToLocal {
            to_exch: to_exch.clone(),
            to_local: to_local.clone(),
            order_latency: order_latency.clone(),
        },
        LocalToExch {
            to_exch,
            to_local,
            order_latency,
        },
    )
}
