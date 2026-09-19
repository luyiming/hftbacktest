//! In-memory checkpoints for independently continuing a backtest on the same thread.

use std::collections::HashMap;

use thiserror::Error;

use crate::{
    backtest::{
        Backtest, BacktestProcessorState,
        assettype::LinearAsset,
        models::{CommonFees, ConstantLatency, RiskAdverseQueueModel, TradingValueFeeModel},
        order::OrderBus,
        proc::{LocalProcessor, Processor},
    },
    depth::{BTreeMarketDepth, MarketDepth},
};

/// Opts a model into in-memory snapshots.
///
/// `Clone` must copy all mutable simulation state independently, including any random-generator
/// state. Only immutable configuration may be shared. Cloning must not replay events, perform I/O,
/// or share a mutable data cursor. This is deliberately not implemented for every `Clone` type.
pub trait SnapshotState: Clone {}

impl SnapshotState for LinearAsset {}
impl SnapshotState for ConstantLatency {}
impl SnapshotState for TradingValueFeeModel<CommonFees> {}
impl SnapshotState for BTreeMarketDepth {}
impl<MD: Clone> SnapshotState for RiskAdverseQueueModel<MD> {}

/// Errors while capturing or restoring a checkpoint.
#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot is not enabled for {0}")]
    Unsupported(&'static str),
}

/// Preserves connections inside one restored branch without sharing mutable buses with its source.
///
/// A fresh context is used for each snapshot or restore. Custom processor snapshot implementations
/// must use this same context for both ends of each order bus.
#[derive(Default)]
pub struct SnapshotContext {
    pub(crate) buses: HashMap<usize, OrderBus>,
}

pub(crate) type LocalSnapshotFn<P, MD> =
    fn(&P, &mut SnapshotContext) -> Box<dyn LocalProcessor<MD>>;
pub(crate) type ProcessorSnapshotFn<P> = fn(&P, &mut SnapshotContext) -> Box<dyn Processor>;

/// An opaque, reusable in-memory checkpoint; not a serialized or cross-thread snapshot.
///
/// Market event buffers are shared, while books, orders, queue positions, in-flight messages,
/// accounting, scheduling and reader cursors are independent. Strategy-owned state must be saved
/// separately at the same decision boundary. Files must remain unchanged while a snapshot lives.
pub struct BacktestSnapshot<MD> {
    state: Backtest<MD>,
}

impl<MD: MarketDepth> BacktestSnapshot<MD> {
    /// Creates an independent replay at the checkpoint, without replaying its history.
    ///
    /// The checkpoint is reusable and survives advancing or dropping its original backtest.
    pub fn restore(&self) -> Result<Backtest<MD>, SnapshotError> {
        self.state.snapshot_state()
    }

    /// Returns the exact saved simulation timestamp.
    pub fn timestamp(&self) -> i64 {
        self.state.cur_ts
    }
}

impl<MD: MarketDepth> Backtest<MD> {
    /// Creates an independent same-thread branch at the current state without advancing this replay.
    ///
    /// This has the same model requirements and state isolation as [`Self::snapshot`], but copies
    /// mutable state only once instead of capturing and then restoring a temporary checkpoint.
    /// The branch survives dropping this replay. Strategy-owned state must be copied separately.
    /// Use [`Self::snapshot`] when several branches should reuse a saved checkpoint later.
    pub fn fork(&self) -> Result<Self, SnapshotError> {
        self.snapshot_state()
    }

    /// Captures the state between public bot calls without advancing time or reading feed files.
    ///
    /// Assets must be built with `L2AssetBuilder::build_snapshotable`, or supply processors that
    /// implement the snapshot hooks. Pending requests and responses need not be drained first.
    /// Cost is proportional to mutable state, not to the length of the historical prefix.
    pub fn snapshot(&self) -> Result<BacktestSnapshot<MD>, SnapshotError> {
        Ok(BacktestSnapshot {
            state: self.snapshot_state()?,
        })
    }

    fn snapshot_state(&self) -> Result<Self, SnapshotError> {
        let mut context = SnapshotContext::default();
        let local = self
            .local
            .iter()
            .map(|state| Ok(state.snapshot_with(state.processor.snapshot_local(&mut context)?)))
            .collect::<Result<Vec<_>, SnapshotError>>()?;
        let exch = self
            .exch
            .iter()
            .map(|state| Ok(state.snapshot_with(state.processor.snapshot_processor(&mut context)?)))
            .collect::<Result<Vec<_>, SnapshotError>>()?;
        Ok(Self {
            cur_ts: self.cur_ts,
            evs: self.evs.snapshot(),
            local,
            exch,
        })
    }
}

impl<P: Processor> BacktestProcessorState<P> {
    fn snapshot_with<Q: Processor>(&self, processor: Q) -> BacktestProcessorState<Q> {
        BacktestProcessorState {
            data: self.data.clone(),
            processor,
            reader: self.reader.clone(),
            row: self.row,
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::*;
    use crate::backtest::{
        L2AssetBuilder,
        rules::{TickSizeChange, TickSizeSchedule},
    };

    #[test]
    fn snapshot_restore_keeps_standard_reader_state_shareable() {
        type Fees = TradingValueFeeModel<CommonFees>;
        let schedule = TickSizeSchedule::new(vec![TickSizeChange {
            effective_from: 0,
            tick_size: Decimal::new(1, 2),
        }])
        .unwrap();
        let asset = L2AssetBuilder::<
            ConstantLatency,
            LinearAsset,
            RiskAdverseQueueModel<BTreeMarketDepth>,
            BTreeMarketDepth,
            Fees,
        >::new()
        .data(Vec::new())
        .latency_model(ConstantLatency::new(0, 0))
        .asset_type(LinearAsset::new(1.0))
        .fee_model(TradingValueFeeModel::new(CommonFees::new(0.0, 0.0)))
        .queue_model(RiskAdverseQueueModel::new())
        .depth(BTreeMarketDepth::new)
        .tick_size_schedule(schedule)
        .build_snapshotable()
        .unwrap();
        let backtest = Backtest::<BTreeMarketDepth>::builder()
            .add_asset(asset)
            .build()
            .unwrap();
        let snapshot = backtest.snapshot().unwrap();
        let restored = snapshot.restore().unwrap();
        assert_eq!(snapshot.timestamp(), restored.cur_ts);
    }
}
