use std::{collections::BTreeMap, error::Error};

use crate::{
    backtest::{
        Asset, Backtest, DataSource, ExchangeKind, L2AssetBuilder,
        assettype::LinearAsset,
        data::Data,
        models::{CommonFees, ConstantLatency, RiskAdverseQueueModel, TradingValueFeeModel},
        proc::{LocalProcessor, Processor},
        snapshot::SnapshotError,
    },
    prelude::*,
};

type TestResult = Result<(), Box<dyn Error>>;
type TestAsset = Asset<dyn LocalProcessor<BTreeMarketDepth>, dyn Processor, Event>;

mod files;
mod performance;

fn event(timestamp: i64, flag: u64, price: f64, qty: f64) -> Event {
    Event {
        ev: flag | EXCH_EVENT | LOCAL_EVENT,
        exch_ts: timestamp,
        local_ts: timestamp,
        px: price,
        qty,
        order_id: 0,
        ival: 0,
        fval: 0.0,
    }
}

fn events() -> Vec<Event> {
    vec![
        event(0, DEPTH_SNAPSHOT_EVENT | BUY_EVENT, 100.0, 10.0),
        event(0, DEPTH_SNAPSHOT_EVENT | SELL_EVENT, 101.0, 10.0),
        event(0, DEPTH_EVENT | BUY_EVENT, 99.0, 15.0),
        event(0, DEPTH_EVENT | SELL_EVENT, 102.0, 15.0),
        event(10, TRADE_EVENT | SELL_EVENT, 100.0, 12.0),
        event(12, TRADE_EVENT | BUY_EVENT, 101.0, 12.0),
        event(20, TRADE_EVENT | SELL_EVENT, 100.0, 1.0),
        event(22, TRADE_EVENT | BUY_EVENT, 101.0, 1.0),
        event(30, TRADE_EVENT | SELL_EVENT, 100.0, 10.0),
        event(32, TRADE_EVENT | BUY_EVENT, 101.0, 10.0),
        event(45, DEPTH_EVENT | BUY_EVENT, 100.0, 7.0),
        event(45, DEPTH_EVENT | SELL_EVENT, 101.0, 8.0),
        event(60, 0, 0.0, 0.0),
    ]
}

fn asset(sources: Vec<DataSource<Event>>, latency: i64) -> Result<TestAsset, Box<dyn Error>> {
    Ok(L2AssetBuilder::new()
        .data(sources)
        .parallel_load(true)
        .latency_model(ConstantLatency::new(latency, latency))
        .asset_type(LinearAsset::new(1.0))
        .fee_model(TradingValueFeeModel::new(CommonFees::new(0.00009, 0.00027)))
        .queue_model(RiskAdverseQueueModel::new())
        .exchange(ExchangeKind::PartialFillExchange)
        .last_trades_capacity(8)
        .depth(|| BTreeMarketDepth::new(1.0, 1.0))
        .build_snapshotable()?)
}

fn replay(latency: i64) -> Result<Backtest<BTreeMarketDepth>, Box<dyn Error>> {
    let mut hbt = Backtest::builder()
        .add_asset(asset(
            vec![DataSource::Data(Data::from_data(&events()))],
            latency,
        )?)
        .build()?;
    hbt.elapse(0)?;
    Ok(hbt)
}

fn advance(hbt: &mut Backtest<BTreeMarketDepth>, timestamp: i64) -> TestResult {
    assert!(timestamp >= hbt.current_timestamp());
    hbt.elapse(timestamp - hbt.current_timestamp())?;
    Ok(())
}

#[derive(Debug, PartialEq)]
struct AssetView {
    bids: BTreeMap<i64, f64>,
    asks: BTreeMap<i64, f64>,
    depth_state: (i64, i64, i64, bool),
    account: StateValues,
    orders: BTreeMap<u64, (String, Option<f64>)>,
    trades: Vec<Event>,
    feed_latency: Option<(i64, i64)>,
    order_latency: Option<(i64, i64, i64)>,
}

fn view(hbt: &Backtest<BTreeMarketDepth>) -> (i64, Vec<AssetView>) {
    (
        hbt.current_timestamp(),
        (0..hbt.num_assets())
            .map(|asset| {
                let depth = hbt.depth(asset);
                AssetView {
                    bids: depth.bid_depth.clone(),
                    asks: depth.ask_depth.clone(),
                    depth_state: (
                        depth.best_bid_tick,
                        depth.best_ask_tick,
                        depth.timestamp,
                        depth.depth_ready,
                    ),
                    account: hbt.state_values(asset).clone(),
                    orders: hbt
                        .orders(asset)
                        .iter()
                        .map(|(id, order)| {
                            (
                                *id,
                                (
                                    format!("{order:?}"),
                                    order.q.as_any().downcast_ref::<f64>().copied(),
                                ),
                            )
                        })
                        .collect(),
                    trades: hbt.last_trades(asset).to_vec(),
                    feed_latency: hbt.feed_latency(asset),
                    order_latency: hbt.order_latency(asset),
                }
            })
            .collect(),
    )
}

#[test]
fn partial_fills_queue_and_total_quantity_amendments_survive_restore() -> TestResult {
    for side in [Side::Buy, Side::Sell] {
        let mut original = replay(0)?;
        original.submit_order(
            0,
            OrderRequest {
                order_id: 1,
                price: if side == Side::Buy { 100.0 } else { 101.0 },
                price_match: PriceMatch::None,
                qty: 5.0,
                side,
                time_in_force: TimeInForce::GTX,
                order_type: OrdType::Limit,
            },
            true,
        )?;
        advance(&mut original, 15)?;
        assert_eq!(original.orders(0)[&1].cum_exec_qty, 2.0);
        let before = view(&original);
        let snapshot = original.snapshot()?;
        assert_eq!(view(&original), before);
        let mut restored = snapshot.restore()?;
        assert_eq!(view(&original), view(&restored));
        for hbt in [&mut original, &mut restored] {
            let price = hbt.orders(0)[&1].price();
            hbt.modify(0, 1, price, 4.0, true)?;
        }
        for timestamp in [21, 25, 35, 50] {
            advance(&mut original, timestamp)?;
            advance(&mut restored, timestamp)?;
            assert_eq!(view(&original), view(&restored));
        }
        let order = &original.orders(0)[&1];
        // The exchange requeues amendments; restoring must preserve this existing behavior.
        assert_eq!(
            (order.qty, order.cum_exec_qty, order.leaves_qty),
            (4.0, 3.0, 1.0)
        );
        assert_eq!(view(&snapshot.restore()?), before);
    }
    Ok(())
}

#[test]
fn fork_matches_checkpoint_with_pending_orders_after_source_drop() -> TestResult {
    for timestamp in [0, 11, 15] {
        let mut original = replay(3)?;
        original.submit_buy_order(0, 1, 100.0, 5.0, TimeInForce::GTX, OrdType::Limit, false)?;
        advance(&mut original, timestamp)?;
        let before = view(&original);
        let mut forked = original.fork()?;
        assert_eq!(view(&original), before);
        assert_eq!(view(&forked), before);
        let mut restored = original.snapshot()?.restore()?;
        drop(original);
        for next_timestamp in [timestamp + 1, 25, 35, 50] {
            advance(&mut forked, next_timestamp)?;
            advance(&mut restored, next_timestamp)?;
            assert_eq!(view(&forked), view(&restored));
        }
        assert_eq!(forked.position(0), 5.0);
    }
    Ok(())
}

#[test]
fn fork_can_cancel_or_take_without_changing_the_source() -> TestResult {
    let mut original = replay(0)?;
    original.submit_buy_order(0, 1, 100.0, 5.0, TimeInForce::GTX, OrdType::Limit, true)?;
    let snapshot = original.snapshot()?;
    let before = view(&original);
    let mut canceled = original.fork()?;
    canceled.cancel(0, 1, true)?;
    canceled.submit_buy_order(0, 2, 0.0, 12.0, TimeInForce::IOC, OrdType::Market, true)?;
    assert_eq!(canceled.orders(0)[&2].taker_price_level_count, 2);
    advance(&mut canceled, 50)?;
    assert_eq!(view(&original), before);
    let mut untouched = snapshot.restore()?;
    advance(&mut original, 50)?;
    advance(&mut untouched, 50)?;
    assert_eq!(view(&original), view(&untouched));
    assert_eq!((original.position(0), canceled.position(0)), (5.0, 12.0));
    drop(original);
    drop(canceled);
    drop(untouched);
    let mut late_restore = snapshot.restore()?;
    advance(&mut late_restore, 50)?;
    assert_eq!(late_restore.position(0), 5.0);
    Ok(())
}

#[test]
fn pending_requests_and_exchange_responses_are_independent() -> TestResult {
    let mut original = replay(3)?;
    original.submit_buy_order(0, 1, 100.0, 5.0, TimeInForce::GTX, OrdType::Limit, false)?;
    let mut restored = original.snapshot()?.restore()?;
    for timestamp in [2, 4, 7, 11] {
        advance(&mut restored, timestamp)?;
        advance(&mut original, timestamp)?;
        assert_eq!(view(&original), view(&restored));
    }
    // The exchange filled at 10, but the response will not reach the local until 13.
    assert_eq!(original.position(0), 0.0);
    let pending_response = original.snapshot()?;
    let mut response_branch = pending_response.restore()?;
    advance(&mut response_branch, 15)?;
    assert_eq!(
        (original.position(0), response_branch.position(0)),
        (0.0, 2.0)
    );
    advance(&mut original, 15)?;
    assert_eq!(view(&original), view(&response_branch));
    original.modify(0, 1, 100.0, 4.0, false)?;
    let mut amendment = original.snapshot()?.restore()?;
    advance(&mut amendment, 25)?;
    advance(&mut original, 25)?;
    assert_eq!(view(&original), view(&amendment));
    original.cancel(0, 1, false)?;
    let mut cancellation = original.snapshot()?.restore()?;
    advance(&mut cancellation, 40)?;
    advance(&mut original, 40)?;
    assert_eq!(view(&original), view(&cancellation));
    Ok(())
}

#[test]
fn empty_trade_buffers_keep_their_retention_capacity() -> TestResult {
    let mut original = replay(0)?;
    let mut restored = original.snapshot()?.restore()?;
    advance(&mut original, 15)?;
    advance(&mut restored, 15)?;
    assert_eq!(view(&original), view(&restored));
    assert_eq!(restored.last_trades(0).len(), 2);
    original.clear_last_trades(Some(0));
    let mut empty_again = original.snapshot()?.restore()?;
    advance(&mut original, 40)?;
    advance(&mut empty_again, 40)?;
    assert_eq!(view(&original), view(&empty_again));
    assert_eq!(empty_again.last_trades(0).len(), 4);
    Ok(())
}

#[test]
fn memory_chunks_can_be_revisited_after_source_finishes_and_is_dropped() -> TestResult {
    let feed = events();
    let sources = feed
        .chunks(3)
        .map(|chunk| DataSource::Data(Data::from_data(chunk)))
        .collect();
    let mut original = Backtest::builder().add_asset(asset(sources, 0)?).build()?;
    original.elapse(0)?;
    let snapshot = original.snapshot()?;
    original.goto_end()?;
    let expected = view(&original);
    drop(original);
    for _ in 0..5 {
        let mut restored = snapshot.restore()?;
        restored.goto_end()?;
        assert_eq!(view(&restored), expected);
    }
    Ok(())
}

#[test]
fn initialized_empty_and_end_of_data_states_restore() -> TestResult {
    let mut original = Backtest::builder()
        .add_asset(asset(
            vec![DataSource::Data(Data::from_data(&events()))],
            0,
        )?)
        .build()?;
    let mut restored = original.snapshot()?.restore()?;
    original.elapse(0)?;
    restored.elapse(0)?;
    assert_eq!(view(&original), view(&restored));
    original.goto_end()?;
    let mut at_end = original.snapshot()?.restore()?;
    assert_eq!(at_end.elapse(100)?, ElapseResult::EndOfData);
    assert_eq!(view(&original), view(&at_end));
    let mut empty = Backtest::builder()
        .add_asset(asset(Vec::new(), 0)?)
        .build()?;
    assert_eq!(empty.elapse(0)?, ElapseResult::EndOfData);
    assert_eq!(
        empty.snapshot()?.restore()?.elapse(0)?,
        ElapseResult::EndOfData
    );
    let mut empty_chunk = Backtest::builder()
        .add_asset(asset(vec![DataSource::Data(Data::empty())], 0)?)
        .build()?;
    empty_chunk.elapse(0)?;
    let snapshot = empty_chunk.snapshot()?;
    for _ in 0..4 {
        assert_eq!(snapshot.restore()?.elapse(0)?, ElapseResult::EndOfData);
    }
    Ok(())
}

#[test]
fn event_cursors_and_simultaneous_multi_asset_events_restore() -> TestResult {
    let mut feed = events();
    for event in &mut feed {
        event.local_ts += 2;
    }
    let mut original = Backtest::builder()
        .add_asset(asset(vec![DataSource::Data(Data::from_data(&feed))], 1)?)
        .add_asset(asset(vec![DataSource::Data(Data::from_data(&feed))], 1)?)
        .build()?;
    original.elapse(3)?;
    for asset in 0..2 {
        original.submit_buy_order(
            asset,
            1,
            100.0,
            5.0,
            TimeInForce::GTX,
            OrdType::Limit,
            false,
        )?;
    }
    advance(&mut original, 11)?;
    let mut restored = original.snapshot()?.restore()?;
    for timestamp in [12, 13, 20, 21, 22, 35, 50] {
        advance(&mut original, timestamp)?;
        advance(&mut restored, timestamp)?;
        assert_eq!(view(&original), view(&restored));
    }
    assert_eq!((original.position(0), original.position(1)), (5.0, 5.0));
    Ok(())
}

#[test]
fn shared_feed_buffers_separate_on_mutation() -> TestResult {
    let mut data = Data::from_data(&events());
    let mut original = Backtest::builder()
        .add_asset(asset(vec![DataSource::Data(data.clone())], 0)?)
        .build()?;
    original.elapse(0)?;
    let mut restored = original.snapshot()?.restore()?;
    data[10].qty = 123.0;
    advance(&mut original, 50)?;
    advance(&mut restored, 50)?;
    assert_eq!(view(&original), view(&restored));
    assert_eq!(restored.depth(0).bid_qty_at_tick(100), 7.0);
    assert_eq!(data[10].qty, 123.0);
    Ok(())
}

#[test]
fn ordinary_build_explicitly_rejects_snapshot_and_fork() -> TestResult {
    let original = Backtest::builder()
        .add_asset(
            L2AssetBuilder::new()
                .data(vec![DataSource::Data(Data::from_data(&events()))])
                .latency_model(ConstantLatency::new(0, 0))
                .asset_type(LinearAsset::new(1.0))
                .fee_model(TradingValueFeeModel::new(CommonFees::new(0.0, 0.0)))
                .queue_model(RiskAdverseQueueModel::new())
                .depth(|| BTreeMarketDepth::new(1.0, 1.0))
                .build()?,
        )
        .build()?;
    assert!(matches!(
        original.snapshot(),
        Err(SnapshotError::Unsupported(_))
    ));
    assert!(matches!(
        original.fork(),
        Err(SnapshotError::Unsupported(_))
    ));
    Ok(())
}

#[test]
fn no_partial_fill_exchange_can_restore_pending_orders() -> TestResult {
    let mut original = Backtest::builder()
        .add_asset(
            L2AssetBuilder::new()
                .data(vec![DataSource::Data(Data::from_data(&events()))])
                .latency_model(ConstantLatency::new(2, 2))
                .asset_type(LinearAsset::new(1.0))
                .fee_model(TradingValueFeeModel::new(CommonFees::new(0.00009, 0.00027)))
                .queue_model(RiskAdverseQueueModel::new())
                .exchange(ExchangeKind::NoPartialFillExchange)
                .last_trades_capacity(8)
                .depth(|| BTreeMarketDepth::new(1.0, 1.0))
                .build_snapshotable()?,
        )
        .build()?;
    original.elapse(0)?;
    original.submit_sell_order(0, 1, 101.0, 5.0, TimeInForce::GTX, OrdType::Limit, false)?;
    let mut restored = original.snapshot()?.restore()?;
    advance(&mut original, 50)?;
    advance(&mut restored, 50)?;
    assert_eq!(view(&original), view(&restored));
    assert_eq!(restored.position(0), -5.0);
    Ok(())
}
