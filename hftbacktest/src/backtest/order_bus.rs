use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use crate::{
    backtest::{models::LatencyModel, snapshot::SnapshotContext},
    types::{OrderRequest, OrderUpdate},
};

#[derive(Clone, Debug)]
enum OrderMessage {
    Request(OrderRequest),
    Update(OrderUpdate),
}

/// Provides a time-ordered bus for order requests and updates.
#[derive(Clone, Debug, Default)]
pub struct OrderBus {
    messages: Rc<RefCell<VecDeque<(OrderMessage, i64)>>>,
}

impl OrderBus {
    pub fn snapshot(&self, context: &mut SnapshotContext) -> Self {
        let key = Rc::as_ptr(&self.messages) as usize;
        context
            .buses
            .entry(key)
            .or_insert_with(|| Self {
                messages: Rc::new(RefCell::new(self.messages.borrow().clone())),
            })
            .clone()
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn earliest_timestamp(&self) -> Option<i64> {
        self.messages
            .borrow()
            .front()
            .map(|(_, timestamp)| *timestamp)
    }

    fn append(&mut self, message: OrderMessage, timestamp: i64) {
        let mut messages = self.messages.borrow_mut();
        let latest_timestamp = messages.back().map_or(0, |(_, timestamp)| *timestamp);
        messages.push_back((message, timestamp.max(latest_timestamp)));
    }

    pub fn reset(&mut self) {
        self.messages.borrow_mut().clear();
    }

    pub fn len(&self) -> usize {
        self.messages.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.borrow().is_empty()
    }

    fn pop_front(&mut self) -> Option<(OrderMessage, i64)> {
        self.messages.borrow_mut().pop_front()
    }
}

/// Exchange-side endpoint of the order bus.
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

    pub fn earliest_recv_order_timestamp(&self) -> Option<i64> {
        self.to_exch.earliest_timestamp()
    }

    pub fn earliest_send_order_timestamp(&self) -> Option<i64> {
        self.to_local.earliest_timestamp()
    }

    pub fn respond(&mut self, update: OrderUpdate) {
        let response_latency = self.order_latency.response(update.exch_timestamp, &update);
        assert!(
            response_latency >= 0,
            "order response latency must be nonnegative"
        );
        let local_recv_timestamp = update.exch_timestamp + response_latency;
        self.to_local
            .append(OrderMessage::Update(update), local_recv_timestamp);
    }

    pub fn receive(&mut self, receipt_timestamp: i64) -> Option<OrderRequest> {
        receive_message(&mut self.to_exch, receipt_timestamp).map(|message| match message {
            OrderMessage::Request(request) => request,
            OrderMessage::Update(_) => unreachable!("exchange bus only receives requests"),
        })
    }
}

/// Local-side endpoint of the order bus.
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

    pub fn earliest_recv_order_timestamp(&self) -> Option<i64> {
        self.to_local.earliest_timestamp()
    }

    pub fn earliest_send_order_timestamp(&self) -> Option<i64> {
        self.to_exch.earliest_timestamp()
    }

    pub fn request(&mut self, request: OrderRequest) {
        let entry_latency = self
            .order_latency
            .entry(request.local_timestamp(), &request);
        assert!(
            entry_latency >= 0,
            "order entry latency must be nonnegative"
        );
        let receive_timestamp = request.local_timestamp() + entry_latency;
        self.to_exch
            .append(OrderMessage::Request(request), receive_timestamp);
    }

    pub fn receive(&mut self, receipt_timestamp: i64) -> Option<OrderUpdate> {
        receive_message(&mut self.to_local, receipt_timestamp).map(|message| match message {
            OrderMessage::Update(update) => update,
            OrderMessage::Request(_) => unreachable!("local bus only receives updates"),
        })
    }
}

fn receive_message(bus: &mut OrderBus, receipt_timestamp: i64) -> Option<OrderMessage> {
    let timestamp = bus.earliest_timestamp()?;
    if timestamp == receipt_timestamp {
        bus.pop_front().map(|(message, _)| message)
    } else {
        assert!(timestamp > receipt_timestamp);
        None
    }
}

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
