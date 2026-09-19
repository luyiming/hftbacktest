use std::{
    collections::HashMap,
    io::Error as IoError,
    ops::{Deref, DerefMut},
    sync::Arc,
};

pub use data::DataSource;
use data::Reader;
use models::FeeModel;
use rust_decimal::Decimal;
use thiserror::Error;

use crate::{
    backtest::{
        assettype::AssetType,
        data::{adjust_feed_latency, format::read_market_data_file},
        evs::{EventIntentKind, EventSet},
        models::{LatencyModel, QueueModel},
        order::order_bus,
        proc::{Local, LocalProcessor, NoPartialFillExchange, PartialFillExchange, Processor},
        state::State,
    },
    depth::{L2MarketDepth, MarketDepth},
    prelude::{
        Bot, OrdType, Order, OrderId, OrderRequest, PriceMatch, Side, StateValues, TimeInForce,
        UNTIL_END_OF_DATA, WaitOrderResponse,
    },
    types::{BuildError, ElapseResult, Event},
};

/// Provides asset types.
pub mod assettype;
pub mod rules;

pub mod models;

/// OrderBus implementation
pub mod order;

/// Local and exchange models
pub mod proc;

/// Trading state.
pub mod state;

/// Recorder for a bot's trading statistics.
pub mod recorder;

pub mod data;
mod evs;

/// In-memory replay checkpoints.
pub mod snapshot;

/// Errors that can occur during backtesting.
#[derive(Error, Debug)]
pub enum BacktestError {
    #[error("Order related to a given order id already exists")]
    OrderIdExist,
    #[error("Order request is in process")]
    OrderRequestInProcess,
    #[error("Order not found")]
    OrderNotFound,
    #[error("order request is invalid")]
    InvalidOrderRequest,
    #[error("order status is invalid to proceed the request")]
    InvalidOrderStatus,
    #[error("end of data")]
    EndOfData,
    #[error("data error: {0:?}")]
    DataError(#[from] IoError),
    #[error("tick size rule error: {0}")]
    TickSize(#[from] rules::TickSizeError),
}

/// Backtesting Asset
pub struct Asset<L: ?Sized, E: ?Sized, D: Clone + Send + Sync + 'static> {
    pub local: Box<L>,
    pub exch: Box<E>,
    pub reader: Reader<D>,
}

impl<L, E, D: Clone + Send + Sync + 'static> Asset<L, E, D> {
    /// Constructs an instance of `Asset`. Use this method if a custom local processor or an
    /// exchange processor is needed.
    pub fn new(local: L, exch: E, reader: Reader<D>) -> Self {
        Self {
            local: Box::new(local),
            exch: Box::new(exch),
            reader,
        }
    }

    /// Returns an `L2AssetBuilder`.
    pub fn l2_builder<LM, AT, QM, MD, FM>() -> L2AssetBuilder<LM, AT, QM, MD, FM>
    where
        AT: AssetType + Clone + 'static,
        MD: MarketDepth + L2MarketDepth + 'static,
        QM: QueueModel<MD> + 'static,
        LM: LatencyModel + Clone + 'static,
        FM: FeeModel + Clone + 'static,
    {
        L2AssetBuilder::new()
    }
}

/// Exchange model kind.
#[derive(Clone, Copy)]
pub enum ExchangeKind {
    /// Uses [NoPartialFillExchange](`NoPartialFillExchange`).
    NoPartialFillExchange,
    /// Uses [PartialFillExchange](`PartialFillExchange`).
    PartialFillExchange,
}

/// A level-2 asset builder.
pub struct L2AssetBuilder<LM, AT, QM, MD, FM> {
    latency_model: Option<LM>,
    asset_type: Option<AT>,
    data: Vec<DataSource<Event>>,
    parallel_load: bool,
    latency_offset: i64,
    fee_model: Option<FM>,
    exch_kind: ExchangeKind,
    last_trades_cap: usize,
    queue_model: Option<QM>,
    depth_builder: Option<Box<dyn Fn() -> MD>>,
    tick_sizes: Option<rules::TickSizeSchedule>,
}

impl<LM, AT, QM, MD, FM> L2AssetBuilder<LM, AT, QM, MD, FM>
where
    AT: AssetType + Clone + 'static,
    MD: MarketDepth + L2MarketDepth + 'static,
    QM: QueueModel<MD> + 'static,
    LM: LatencyModel + Clone + 'static,
    FM: FeeModel + Clone + 'static,
{
    /// Constructs an `L2AssetBuilder`.
    pub fn new() -> Self {
        Self {
            latency_model: None,
            asset_type: None,
            data: vec![],
            parallel_load: false,
            latency_offset: 0,
            fee_model: None,
            exch_kind: ExchangeKind::NoPartialFillExchange,
            last_trades_cap: 0,
            queue_model: None,
            depth_builder: None,
            tick_sizes: None,
        }
    }

    /// Sets the feed data.
    pub fn data(self, data: Vec<DataSource<Event>>) -> Self {
        Self { data, ..self }
    }

    /// Sets whether to load the next data in parallel with backtesting. This can speed up the
    /// backtest by reducing data loading time, but it also increases memory usage.
    /// The default value is `true`.
    pub fn parallel_load(self, parallel_load: bool) -> Self {
        Self {
            parallel_load,
            ..self
        }
    }

    /// Sets the latency offset to adjust the feed latency by the specified amount. This is
    /// particularly useful in cross-exchange backtesting, where the feed data is collected from a
    /// different site than the one where the strategy is intended to run.
    pub fn latency_offset(self, latency_offset: i64) -> Self {
        Self {
            latency_offset,
            ..self
        }
    }

    /// Sets a latency model.
    pub fn latency_model(self, latency_model: LM) -> Self {
        Self {
            latency_model: Some(latency_model),
            ..self
        }
    }

    /// Sets an asset type.
    pub fn asset_type(self, asset_type: AT) -> Self {
        Self {
            asset_type: Some(asset_type),
            ..self
        }
    }

    /// Sets a fee model.
    pub fn fee_model(self, fee_model: FM) -> Self {
        Self {
            fee_model: Some(fee_model),
            ..self
        }
    }

    /// Sets an exchange model. The default value is [`NoPartialFillExchange`].
    pub fn exchange(self, exch_kind: ExchangeKind) -> Self {
        Self { exch_kind, ..self }
    }

    /// Sets the initial capacity of the vector storing the last market trades.
    /// The default value is `0`, indicating that no last trades are stored.
    pub fn last_trades_capacity(self, capacity: usize) -> Self {
        Self {
            last_trades_cap: capacity,
            ..self
        }
    }

    /// Sets a queue model.
    pub fn queue_model(self, queue_model: QM) -> Self {
        Self {
            queue_model: Some(queue_model),
            ..self
        }
    }

    /// Sets a market depth builder.
    pub fn depth<Builder>(self, builder: Builder) -> Self
    where
        Builder: Fn() -> MD + 'static,
    {
        Self {
            depth_builder: Some(Box::new(builder)),
            ..self
        }
    }

    /// Sets the time-varying tick-size rules used to validate explicit order prices.
    pub fn tick_size_schedule(self, tick_sizes: rules::TickSizeSchedule) -> Self {
        Self {
            tick_sizes: Some(tick_sizes),
            ..self
        }
    }

    /// Builds an `Asset`.
    pub fn build(self) -> Result<Asset<dyn LocalProcessor<MD>, dyn Processor, Event>, BuildError> {
        self.build_with(|local| local, |exch| exch, |exch| exch)
    }

    /// Builds an L2 asset with independent in-memory snapshot support.
    ///
    /// Models explicitly opt into `SnapshotState`; ordinary `Clone` alone does not guarantee
    /// independent simulation state. Existing `build` behavior is unchanged.
    pub fn build_snapshotable(
        self,
    ) -> Result<Asset<dyn LocalProcessor<MD>, dyn Processor, Event>, BuildError>
    where
        AT: snapshot::SnapshotState,
        LM: snapshot::SnapshotState,
        MD: snapshot::SnapshotState,
        QM: snapshot::SnapshotState,
        FM: snapshot::SnapshotState,
    {
        self.build_with(
            |local| local.enable_snapshot(),
            |exch| exch.enable_snapshot(),
            |exch| exch.enable_snapshot(),
        )
    }

    fn build_with(
        self,
        prepare_local: impl FnOnce(Local<AT, LM, MD, FM>) -> Local<AT, LM, MD, FM>,
        prepare_no_partial: impl FnOnce(
            NoPartialFillExchange<AT, LM, QM, MD, FM>,
        ) -> NoPartialFillExchange<AT, LM, QM, MD, FM>,
        prepare_partial: impl FnOnce(
            PartialFillExchange<AT, LM, QM, MD, FM>,
        ) -> PartialFillExchange<AT, LM, QM, MD, FM>,
    ) -> Result<Asset<dyn LocalProcessor<MD>, dyn Processor, Event>, BuildError> {
        let reader = if self.latency_offset == 0 {
            Reader::builder(read_market_data_file)
                .parallel_load(self.parallel_load)
                .data(self.data)
                .build()
                .map_err(|err| BuildError::Error(err.into()))?
        } else {
            Reader::builder(read_market_data_file)
                .parallel_load(self.parallel_load)
                .data(self.data)
                .preprocess(move |data| adjust_feed_latency(data, self.latency_offset))
                .build()
                .map_err(|err| BuildError::Error(err.into()))?
        };

        let create_depth = self
            .depth_builder
            .as_ref()
            .ok_or(BuildError::BuilderIncomplete("depth"))?;
        let order_latency = self
            .latency_model
            .clone()
            .ok_or(BuildError::BuilderIncomplete("order_latency"))?;
        let asset_type = self
            .asset_type
            .clone()
            .ok_or(BuildError::BuilderIncomplete("asset_type"))?;
        let fee_model = self
            .fee_model
            .clone()
            .ok_or(BuildError::BuilderIncomplete("fee_model"))?;
        let tick_sizes = self
            .tick_sizes
            .clone()
            .ok_or(BuildError::BuilderIncomplete("tick_size_schedule"))?;

        let (order_e2l, order_l2e) = order_bus(order_latency);

        let local = prepare_local(Local::new(
            create_depth(),
            State::new(asset_type, fee_model),
            self.last_trades_cap,
            order_l2e,
        ));

        let queue_model = self
            .queue_model
            .ok_or(BuildError::BuilderIncomplete("queue_model"))?;
        let asset_type = self
            .asset_type
            .clone()
            .ok_or(BuildError::BuilderIncomplete("asset_type"))?;
        let fee_model = self
            .fee_model
            .clone()
            .ok_or(BuildError::BuilderIncomplete("fee_model"))?;

        match self.exch_kind {
            ExchangeKind::NoPartialFillExchange => {
                let exch = prepare_no_partial(NoPartialFillExchange::new(
                    create_depth(),
                    State::new(asset_type, fee_model),
                    queue_model,
                    order_e2l,
                    tick_sizes,
                ));

                Ok(Asset {
                    local: Box::new(local),
                    exch: Box::new(exch),
                    reader,
                })
            }
            ExchangeKind::PartialFillExchange => {
                let exch = prepare_partial(PartialFillExchange::new(
                    create_depth(),
                    State::new(asset_type, fee_model),
                    queue_model,
                    order_e2l,
                    tick_sizes,
                ));

                Ok(Asset {
                    local: Box::new(local),
                    exch: Box::new(exch),
                    reader,
                })
            }
        }
    }
}

impl<LM, AT, QM, MD, FM> Default for L2AssetBuilder<LM, AT, QM, MD, FM>
where
    AT: AssetType + Clone + 'static,
    MD: MarketDepth + L2MarketDepth + 'static,
    QM: QueueModel<MD> + 'static,
    LM: LatencyModel + Clone + 'static,
    FM: FeeModel + Clone + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// [`Backtest`] builder.
pub struct BacktestBuilder<MD> {
    local: Vec<BacktestProcessorState<Box<dyn LocalProcessor<MD>>>>,
    exch: Vec<BacktestProcessorState<Box<dyn Processor>>>,
}

impl<MD> BacktestBuilder<MD> {
    /// Adds [`Asset`], which will undergo simulation within the backtester.
    pub fn add_asset(self, asset: Asset<dyn LocalProcessor<MD>, dyn Processor, Event>) -> Self {
        let mut self_ = Self { ..self };
        self_.local.push(BacktestProcessorState::new(
            asset.local,
            asset.reader.clone(),
        ));
        self_
            .exch
            .push(BacktestProcessorState::new(asset.exch, asset.reader));
        self_
    }

    /// Builds [`Backtest`].
    pub fn build(self) -> Result<Backtest<MD>, BuildError> {
        let num_assets = self.local.len();
        if self.local.len() != num_assets || self.exch.len() != num_assets {
            panic!();
        }
        Ok(Backtest {
            cur_ts: i64::MAX,
            evs: EventSet::new(num_assets),
            local: self.local,
            exch: self.exch,
        })
    }
}

/// This backtester provides multi-asset and multi-exchange model backtesting, allowing you to
/// configure different setups such as queue models or asset types for each asset. However, this may
/// result in slightly slower performance compared to [`Backtest`].
pub struct Backtest<MD> {
    cur_ts: i64,
    evs: EventSet,
    local: Vec<BacktestProcessorState<Box<dyn LocalProcessor<MD>>>>,
    exch: Vec<BacktestProcessorState<Box<dyn Processor>>>,
}

impl<P: Processor> Deref for BacktestProcessorState<P> {
    type Target = P;

    fn deref(&self) -> &Self::Target {
        &self.processor
    }
}

impl<P: Processor> DerefMut for BacktestProcessorState<P> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.processor
    }
}

/// Per asset backtesting state used internally to advance event buffers.
pub struct BacktestProcessorState<P: Processor> {
    data: Arc<Vec<Event>>,
    processor: P,
    reader: Reader<Event>,
    row: Option<usize>,
}

impl<P: Processor> BacktestProcessorState<P> {
    fn new(processor: P, reader: Reader<Event>) -> BacktestProcessorState<P> {
        Self {
            data: Arc::new(Vec::new()),
            processor,
            reader,
            row: None,
        }
    }

    /// Get the index of the next available row, only advancing the reader if there's no
    /// row currently available.
    fn next_row(&mut self) -> Result<usize, BacktestError> {
        if self.row.is_none() {
            let _ = self.advance()?;
        }

        self.row.ok_or(BacktestError::EndOfData)
    }

    /// Advance the state of this processor to the next available event and return the
    /// timestamp it occurred at, if any.
    fn advance(&mut self) -> Result<i64, BacktestError> {
        loop {
            let start = self.row.map(|rn| rn + 1).unwrap_or(0);

            for rn in start..self.data.len() {
                if let Some(ts) = self.processor.event_seen_timestamp(&self.data[rn]) {
                    self.row = Some(rn);
                    return Ok(ts);
                }
            }

            let next = self.reader.next_data()?;

            self.data = next;
            self.row = None;
        }
    }
}

impl<MD> Backtest<MD>
where
    MD: MarketDepth,
{
    pub fn builder() -> BacktestBuilder<MD> {
        BacktestBuilder {
            local: vec![],
            exch: vec![],
        }
    }

    pub fn new(
        local: Vec<Box<dyn LocalProcessor<MD>>>,
        exch: Vec<Box<dyn Processor>>,
        reader: Vec<Reader<Event>>,
    ) -> Self {
        let num_assets = local.len();
        if local.len() != num_assets || exch.len() != num_assets || reader.len() != num_assets {
            panic!();
        }

        let local = local
            .into_iter()
            .zip(reader.iter())
            .map(|(proc, reader)| BacktestProcessorState::new(proc, reader.clone()))
            .collect();
        let exch = exch
            .into_iter()
            .zip(reader.iter())
            .map(|(proc, reader)| BacktestProcessorState::new(proc, reader.clone()))
            .collect();

        Self {
            local,
            exch,
            cur_ts: i64::MAX,
            evs: EventSet::new(num_assets),
        }
    }

    fn initialize_evs(&mut self) -> Result<(), BacktestError> {
        for (asset_no, local) in self.local.iter_mut().enumerate() {
            match local.advance() {
                Ok(ts) => self.evs.update_local_data(asset_no, ts),
                Err(BacktestError::EndOfData) => {
                    self.evs.invalidate_local_data(asset_no);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        for (asset_no, exch) in self.exch.iter_mut().enumerate() {
            match exch.advance() {
                Ok(ts) => self.evs.update_exch_data(asset_no, ts),
                Err(BacktestError::EndOfData) => {
                    self.evs.invalidate_exch_data(asset_no);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn goto_end(&mut self) -> Result<ElapseResult, BacktestError> {
        if self.cur_ts == i64::MAX {
            self.initialize_evs()?;
            match self.evs.next() {
                Some(ev) => {
                    self.cur_ts = ev.timestamp;
                }
                None => {
                    return Ok(ElapseResult::EndOfData);
                }
            }
        }
        self.goto::<false>(UNTIL_END_OF_DATA, WaitOrderResponse::None)
    }

    fn goto<const WAIT_NEXT_FEED: bool>(
        &mut self,
        timestamp: i64,
        wait_order_response: WaitOrderResponse,
    ) -> Result<ElapseResult, BacktestError> {
        let mut result = ElapseResult::Ok;
        let mut timestamp = timestamp;
        for (asset_no, local) in self.local.iter().enumerate() {
            self.evs
                .update_exch_order(asset_no, local.earliest_send_order_timestamp());
            self.evs
                .update_local_order(asset_no, local.earliest_recv_order_timestamp());
        }
        loop {
            match self.evs.next() {
                Some(ev) => {
                    if ev.timestamp > timestamp {
                        self.cur_ts = timestamp;
                        return Ok(result);
                    }
                    match ev.kind {
                        EventIntentKind::LocalData => {
                            let local = unsafe { self.local.get_unchecked_mut(ev.asset_no) };
                            let next = local.next_row().and_then(|row| {
                                local.processor.process(&local.data[row])?;
                                local.advance()
                            });

                            match next {
                                Ok(next_ts) => {
                                    self.evs.update_local_data(ev.asset_no, next_ts);
                                }
                                Err(BacktestError::EndOfData) => {
                                    self.evs.invalidate_local_data(ev.asset_no);
                                }
                                Err(e) => {
                                    return Err(e);
                                }
                            }
                            if WAIT_NEXT_FEED {
                                timestamp = ev.timestamp;
                                result = ElapseResult::MarketFeed;
                            }
                        }
                        EventIntentKind::LocalOrder => {
                            let local = unsafe { self.local.get_unchecked_mut(ev.asset_no) };
                            let wait_order_resp_id = match wait_order_response {
                                WaitOrderResponse::Specified {
                                    asset_no: wait_order_asset_no,
                                    order_id: wait_order_id,
                                } if ev.asset_no == wait_order_asset_no => Some(wait_order_id),
                                _ => None,
                            };
                            if local.process_recv_order(ev.timestamp, wait_order_resp_id)?
                                || wait_order_response == WaitOrderResponse::Any
                            {
                                timestamp = ev.timestamp;
                                if WAIT_NEXT_FEED {
                                    result = ElapseResult::OrderResponse;
                                }
                            }
                            self.evs.update_local_order(
                                ev.asset_no,
                                local.earliest_recv_order_timestamp(),
                            );
                        }
                        EventIntentKind::ExchData => {
                            let exch = unsafe { self.exch.get_unchecked_mut(ev.asset_no) };
                            let next = exch.next_row().and_then(|row| {
                                exch.processor.process(&exch.data[row])?;
                                exch.advance()
                            });

                            match next {
                                Ok(next_ts) => {
                                    self.evs.update_exch_data(ev.asset_no, next_ts);
                                }
                                Err(BacktestError::EndOfData) => {
                                    self.evs.invalidate_exch_data(ev.asset_no);
                                }
                                Err(e) => {
                                    return Err(e);
                                }
                            }
                            self.evs.update_local_order(
                                ev.asset_no,
                                exch.earliest_send_order_timestamp(),
                            );
                        }
                        EventIntentKind::ExchOrder => {
                            let exch = unsafe { self.exch.get_unchecked_mut(ev.asset_no) };
                            let _ = exch.process_recv_order(ev.timestamp, None)?;
                            self.evs.update_exch_order(
                                ev.asset_no,
                                exch.earliest_recv_order_timestamp(),
                            );
                            self.evs.update_local_order(
                                ev.asset_no,
                                exch.earliest_send_order_timestamp(),
                            );
                        }
                    }
                }
                None => {
                    return Ok(ElapseResult::EndOfData);
                }
            }
        }
    }
}

impl<MD> Bot<MD> for Backtest<MD>
where
    MD: MarketDepth,
{
    type Error = BacktestError;

    #[inline]
    fn current_timestamp(&self) -> i64 {
        self.cur_ts
    }

    #[inline]
    fn num_assets(&self) -> usize {
        self.local.len()
    }

    #[inline]
    fn position(&self, asset_no: usize) -> Decimal {
        self.local.get(asset_no).unwrap().position()
    }

    #[inline]
    fn state_values(&self, asset_no: usize) -> &StateValues {
        self.local.get(asset_no).unwrap().state_values()
    }

    fn depth(&self, asset_no: usize) -> &MD {
        self.local.get(asset_no).unwrap().depth()
    }

    fn last_trades(&self, asset_no: usize) -> &[Event] {
        self.local.get(asset_no).unwrap().last_trades()
    }

    #[inline]
    fn clear_last_trades(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(an) => {
                let local = self.local.get_mut(an).unwrap();
                local.clear_last_trades();
            }
            None => {
                for local in self.local.iter_mut() {
                    local.clear_last_trades();
                }
            }
        }
    }

    #[inline]
    fn orders(&self, asset_no: usize) -> &HashMap<u64, Order> {
        self.local.get(asset_no).unwrap().orders()
    }

    #[inline]
    fn submit_buy_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: Decimal,
        qty: Decimal,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order_id,
            Side::Buy,
            price,
            PriceMatch::None,
            qty,
            order_type,
            time_in_force,
            self.cur_ts,
        )?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified { asset_no, order_id },
            );
        }
        Ok(ElapseResult::Ok)
    }

    #[inline]
    fn submit_buy_order_with_price_match(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        qty: Decimal,
        time_in_force: TimeInForce,
        order_type: OrdType,
        price_match: PriceMatch,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error> {
        if price_match == PriceMatch::None {
            return Err(BacktestError::InvalidOrderRequest);
        }
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order_id,
            Side::Buy,
            Decimal::ZERO,
            price_match,
            qty,
            order_type,
            time_in_force,
            self.cur_ts,
        )?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified { asset_no, order_id },
            );
        }
        Ok(ElapseResult::Ok)
    }

    #[inline]
    fn submit_sell_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: Decimal,
        qty: Decimal,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order_id,
            Side::Sell,
            price,
            PriceMatch::None,
            qty,
            order_type,
            time_in_force,
            self.cur_ts,
        )?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified { asset_no, order_id },
            );
        }
        Ok(ElapseResult::Ok)
    }

    #[inline]
    fn submit_sell_order_with_price_match(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        qty: Decimal,
        time_in_force: TimeInForce,
        order_type: OrdType,
        price_match: PriceMatch,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error> {
        if price_match == PriceMatch::None {
            return Err(BacktestError::InvalidOrderRequest);
        }
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order_id,
            Side::Sell,
            Decimal::ZERO,
            price_match,
            qty,
            order_type,
            time_in_force,
            self.cur_ts,
        )?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified { asset_no, order_id },
            );
        }
        Ok(ElapseResult::Ok)
    }

    fn submit_order(
        &mut self,
        asset_no: usize,
        order: OrderRequest,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order.order_id,
            order.side,
            order.price,
            order.price_match,
            order.qty,
            order.order_type,
            order.time_in_force,
            self.cur_ts,
        )?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified {
                    asset_no,
                    order_id: order.order_id,
                },
            );
        }
        Ok(ElapseResult::Ok)
    }

    #[inline]
    fn modify(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: Decimal,
        qty: Decimal,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.modify(order_id, price, PriceMatch::None, qty, self.cur_ts)?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified { asset_no, order_id },
            );
        }
        Ok(ElapseResult::Ok)
    }

    #[inline]
    fn modify_with_price_match(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        qty: Decimal,
        price_match: PriceMatch,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error> {
        if price_match == PriceMatch::None {
            return Err(BacktestError::InvalidOrderRequest);
        }
        let local = self.local.get_mut(asset_no).unwrap();
        local.modify(order_id, Decimal::ZERO, price_match, qty, self.cur_ts)?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified { asset_no, order_id },
            );
        }
        Ok(ElapseResult::Ok)
    }

    #[inline]
    fn cancel(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        wait: bool,
    ) -> Result<ElapseResult, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.cancel(order_id, self.cur_ts)?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified { asset_no, order_id },
            );
        }
        Ok(ElapseResult::Ok)
    }

    #[inline]
    fn clear_inactive_orders(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(asset_no) => {
                self.local
                    .get_mut(asset_no)
                    .unwrap()
                    .clear_inactive_orders();
            }
            None => {
                for local in self.local.iter_mut() {
                    local.clear_inactive_orders();
                }
            }
        }
    }

    #[inline]
    fn wait_order_response(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        timeout: i64,
    ) -> Result<ElapseResult, BacktestError> {
        self.goto::<false>(
            self.cur_ts + timeout,
            WaitOrderResponse::Specified { asset_no, order_id },
        )
    }

    #[inline]
    fn wait_next_feed(
        &mut self,
        include_order_resp: bool,
        timeout: i64,
    ) -> Result<ElapseResult, Self::Error> {
        if self.cur_ts == i64::MAX {
            self.initialize_evs()?;
            match self.evs.next() {
                Some(ev) => {
                    self.cur_ts = ev.timestamp;
                }
                None => {
                    return Ok(ElapseResult::EndOfData);
                }
            }
        }
        if include_order_resp {
            self.goto::<true>(self.cur_ts + timeout, WaitOrderResponse::Any)
        } else {
            self.goto::<true>(self.cur_ts + timeout, WaitOrderResponse::None)
        }
    }

    #[inline]
    fn elapse(&mut self, duration: i64) -> Result<ElapseResult, Self::Error> {
        if self.cur_ts == i64::MAX {
            self.initialize_evs()?;
            match self.evs.next() {
                Some(ev) => {
                    self.cur_ts = ev.timestamp;
                }
                None => {
                    return Ok(ElapseResult::EndOfData);
                }
            }
        }
        self.goto::<false>(self.cur_ts + duration, WaitOrderResponse::None)
    }

    #[inline]
    fn elapse_bt(&mut self, duration: i64) -> Result<ElapseResult, Self::Error> {
        self.elapse(duration)
    }

    #[inline]
    fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    fn feed_latency(&self, asset_no: usize) -> Option<(i64, i64)> {
        self.local.get(asset_no).unwrap().feed_latency()
    }

    #[inline]
    fn order_latency(&self, asset_no: usize) -> Option<(i64, i64, i64)> {
        self.local.get(asset_no).unwrap().order_latency()
    }
}
