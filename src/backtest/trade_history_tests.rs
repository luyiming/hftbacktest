use std::time::Duration;

use rust_decimal::Decimal;

use crate::{
    backtest::{
        Backtest, DataSource, L2AssetBuilder,
        assettype::LinearAsset,
        models::{CommonFees, ConstantLatency, RiskAdverseQueueModel, TradingValueFeeModel},
        rules::{TickSizeChange, TickSizeSchedule},
    },
    depth::BTreeMarketDepth,
    types::{Bot, ElapseResult, Event, LOCAL_BUY_TRADE_EVENT, LOCAL_EVENT},
};

fn event(timestamp: i64, trade: bool) -> Event {
    Event {
        ev: if trade {
            LOCAL_BUY_TRADE_EVENT
        } else {
            LOCAL_EVENT
        },
        exch_ts: timestamp - 100,
        local_ts: timestamp,
        px: Decimal::from(100),
        qty: Decimal::ONE,
    }
}

fn backtest(
    enabled: bool,
    capacity: usize,
    horizon: Option<Duration>,
) -> Backtest<BTreeMarketDepth> {
    let mut builder = Backtest::builder();
    // The second asset keeps the replay alive after the first asset's feed ends.
    for events in [
        vec![
            event(0, false),
            event(1, true),
            event(2, true),
            event(3, true),
        ],
        vec![event(0, false), event(20, false)],
    ] {
        let asset = L2AssetBuilder::new()
            .data(vec![DataSource::from(events)])
            .latency_model(ConstantLatency::new(0, 0))
            .asset_type(LinearAsset::new(Decimal::ONE))
            .fee_model(TradingValueFeeModel::new(CommonFees::new(
                Decimal::ZERO,
                Decimal::ZERO,
            )))
            .queue_model(RiskAdverseQueueModel::new())
            .depth(BTreeMarketDepth::new)
            .tick_size_schedule(
                TickSizeSchedule::new(vec![TickSizeChange {
                    effective_from: 0,
                    tick_size: Decimal::ONE,
                }])
                .expect("tick size schedule should be valid"),
            )
            .record_trades(enabled)
            .last_trades_capacity(capacity)
            .last_trades_horizon(horizon)
            .build_snapshotable()
            .expect("asset should build");
        builder = builder.add_asset(asset);
    }
    builder.build().expect("backtest should build")
}

fn timestamps(hbt: &Backtest<BTreeMarketDepth>) -> Vec<i64> {
    hbt.last_trades(0).map(|event| event.local_ts).collect()
}

#[test]
fn expires_at_local_time_boundary_without_more_trades_on_the_asset() {
    let mut hbt = backtest(true, 1, Some(Duration::from_nanos(2)));
    hbt.clear_last_trades(None);
    assert_eq!(hbt.last_trades_since(0), None);
    hbt.elapse(2).expect("replay should advance");
    assert_eq!(timestamps(&hbt), vec![1, 2]);
    assert_eq!(hbt.last_trades_since(0), Some(0));
    hbt.wait_next_feed(false, 10)
        .expect("replay should advance");
    assert_eq!(timestamps(&hbt), vec![2, 3]);
    hbt.elapse(1).expect("replay should advance");
    assert_eq!(timestamps(&hbt), vec![3]);
    assert_eq!(hbt.last_trades_since(0), Some(2));
    hbt.elapse(1).expect("replay should advance");
    assert_eq!(timestamps(&hbt), Vec::<i64>::new());
    assert_eq!(hbt.last_trades_since(0), Some(3));
    assert_eq!(
        hbt.goto_end().expect("replay should finish"),
        ElapseResult::EndOfData
    );
    assert_eq!(hbt.current_timestamp(), 20);
    assert_eq!(hbt.last_trades_since(0), Some(18));
}

#[test]
fn snapshots_preserve_horizon_and_clear_resets_only_branch_coverage() {
    let mut hbt = backtest(true, 0, Some(Duration::from_nanos(2)));
    hbt.elapse(2).expect("replay should advance");
    let snapshot = hbt.snapshot().expect("snapshot should succeed");
    let mut branch = snapshot.restore().expect("restore should succeed");
    branch.clear_last_trades(Some(0));
    assert_eq!(branch.last_trades_since(0), Some(2));
    assert_eq!(hbt.last_trades_since(0), Some(0));
    assert_eq!(timestamps(&hbt), vec![1, 2]);
    branch.elapse(1).expect("branch should advance");
    assert_eq!(timestamps(&branch), vec![3]);
    branch.elapse(2).expect("branch should advance");
    assert_eq!(timestamps(&branch), Vec::<i64>::new());
    assert_eq!(
        timestamps(&snapshot.restore().expect("restore should succeed")),
        vec![1, 2]
    );
}

#[test]
fn no_horizon_retains_history_beyond_capacity_until_clear() {
    let mut hbt = backtest(true, 1, None);
    hbt.goto_end().expect("replay should finish");
    assert_eq!(timestamps(&hbt), vec![1, 2, 3]);
    assert_eq!(hbt.last_trades_since(0), Some(0));
    hbt.clear_last_trades(None);
    assert_eq!(timestamps(&hbt), Vec::<i64>::new());
    assert_eq!(hbt.last_trades_since(0), Some(20));
}

#[test]
fn disabled_recording_and_zero_horizon_have_distinct_coverage() {
    let mut disabled = backtest(false, 16, None);
    disabled.goto_end().expect("replay should finish");
    disabled.clear_last_trades(None);
    assert_eq!(timestamps(&disabled), Vec::<i64>::new());
    assert_eq!(disabled.last_trades_since(0), None);
    let mut empty_window = backtest(true, 0, Some(Duration::ZERO));
    empty_window.elapse(3).expect("replay should advance");
    assert_eq!(timestamps(&empty_window), Vec::<i64>::new());
    assert_eq!(empty_window.last_trades_since(0), Some(3));
}
