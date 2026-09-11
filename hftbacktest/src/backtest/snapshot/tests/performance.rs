use std::{hint::black_box, time::Instant};

use super::*;

#[test]
#[ignore = "manual snapshot cost measurement; no timing assertion"]
fn snapshot_cost_before_and_after_a_long_prefix() -> TestResult {
    const LEVELS: usize = 1_000;
    const PREFIX_EVENTS: i64 = 200_000;
    const SAMPLES: usize = 200;
    let mut feed = Vec::with_capacity(LEVELS * 2 + PREFIX_EVENTS as usize + 1);
    for level in 0..LEVELS {
        feed.push(event(
            0,
            DEPTH_EVENT | BUY_EVENT,
            100_000.0 - level as f64,
            10.0,
        ));
        feed.push(event(
            0,
            DEPTH_EVENT | SELL_EVENT,
            100_001.0 + level as f64,
            10.0,
        ));
    }
    for timestamp in 1..=PREFIX_EVENTS {
        feed.push(event(timestamp, DEPTH_EVENT | BUY_EVENT, 100_000.0, 10.0));
    }
    feed.push(event(PREFIX_EVENTS + 10, 0, 0.0, 0.0));
    let mut hbt = Backtest::builder()
        .add_asset(asset(vec![DataSource::Data(Data::from_data(&feed))], 1)?)
        .build()?;
    drop(feed);
    hbt.elapse(0)?;
    for id in 1..=16 {
        hbt.submit_buy_order(
            0,
            id,
            100_000.0,
            5.0,
            TimeInForce::GTX,
            OrdType::Limit,
            false,
        )?;
    }
    advance(&mut hbt, 3)?;
    for timestamp in [3, PREFIX_EVENTS] {
        advance(&mut hbt, timestamp)?;
        let capture_start = Instant::now();
        for _ in 0..SAMPLES {
            black_box(hbt.snapshot()?);
        }
        let capture_us = capture_start.elapsed().as_secs_f64() * 1e6 / SAMPLES as f64;
        let snapshot = hbt.snapshot()?;
        let restore_start = Instant::now();
        for _ in 0..SAMPLES {
            black_box(snapshot.restore()?);
        }
        let restore_us = restore_start.elapsed().as_secs_f64() * 1e6 / SAMPLES as f64;
        println!(
            "snapshot_cost prefix_events={timestamp} levels_per_side={LEVELS} orders=16 samples={SAMPLES} capture_drop_mean_us={capture_us:.3} restore_drop_mean_us={restore_us:.3}"
        );
    }
    Ok(())
}
