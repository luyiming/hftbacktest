use std::path::Path;

use anyhow::{Context, Result, ensure};

use crate::{
    backtest::data::{
        format::{StoredEvent, write_market_data_file},
        fuse::FixedFuse,
        tardis::{FeedKind, TardisReader},
    },
    types::{
        BUY_EVENT, DEPTH_CLEAR_EVENT, DEPTH_SNAPSHOT_EVENT, EXCH_EVENT, LOCAL_EVENT, SELL_EVENT,
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SnapshotMode {
    #[default]
    Process,
    Ignore,
    IgnoreSod,
}

pub struct ConvertRequest<'a> {
    pub trades: &'a Path,
    pub depth: &'a Path,
    pub book_ticker: Option<&'a Path>,
    pub output: &'a Path,
    pub snapshot_mode: SnapshotMode,
    pub base_latency: i64,
}

fn read_events(path: &Path, kind: FeedKind) -> Result<Vec<StoredEvent>> {
    let mut reader = TardisReader::open(path, kind)?;
    let mut events = Vec::new();
    while let Some(mut row) = reader.next_events()? {
        events.append(&mut row);
    }
    ensure!(
        events
            .windows(2)
            .all(|pair| pair[0].local_ts <= pair[1].local_ts),
        "local timestamps are out of order in {}",
        path.display()
    );
    Ok(events)
}

/// Converts complete Tardis inputs without tick-size rounding or floating-point parsing.
pub fn convert_fuse(request: ConvertRequest<'_>) -> Result<usize> {
    ensure!(
        request.base_latency >= 0,
        "base latency must be nonnegative"
    );
    let mut output = read_events(request.trades, FeedKind::Trades)?;
    let depth = read_events(request.depth, FeedKind::Depth)?;
    let ticker = request
        .book_ticker
        .map(|path| read_events(path, FeedKind::BookTicker))
        .transpose()?
        .unwrap_or_default();
    output.extend(fuse_events(&depth, &ticker, request.snapshot_mode));
    let output = order_events(output, request.base_latency)?;
    write_market_data_file(request.output, &output)
        .with_context(|| format!("failed to publish {}", request.output.display()))?;
    Ok(output.len())
}

fn fuse_events(
    depth: &[StoredEvent],
    ticker: &[StoredEvent],
    mode: SnapshotMode,
) -> Vec<StoredEvent> {
    let mut fuse = FixedFuse::default();
    let mut output = Vec::new();
    let (mut d, mut t) = (0, 0);
    let mut at_start = true;
    while d < depth.len() || t < ticker.len() {
        if t < ticker.len() && (d == depth.len() || ticker[t].local_ts < depth[d].local_ts) {
            output.extend(fuse.process(ticker[t].clone()));
            t += 1;
        } else if depth[d].ev & 0xff == DEPTH_SNAPSHOT_EVENT {
            let start = d;
            while d < depth.len() && depth[d].ev & 0xff == DEPTH_SNAPSHOT_EVENT {
                d += 1;
            }
            let publish =
                mode != SnapshotMode::Ignore && !(mode == SnapshotMode::IgnoreSod && at_start);
            for side in [BUY_EVENT, SELL_EVENT] {
                let rows: Vec<_> = depth[start..d]
                    .iter()
                    .filter(|row| row.ev & side != 0)
                    .collect();
                if let Some(first) = rows.first() {
                    let limit = if side == BUY_EVENT {
                        rows.iter().map(|row| row.px).min()
                    } else {
                        rows.iter().map(|row| row.px).max()
                    }
                    .expect("nonempty snapshot should have a price limit");
                    let cleared = fuse.process(StoredEvent {
                        ev: DEPTH_CLEAR_EVENT | side,
                        px: limit,
                        qty: 0,
                        ..(*first).clone()
                    });
                    if publish {
                        output.extend(cleared);
                    }
                    for row in rows {
                        let updated = fuse.process(row.clone());
                        if publish {
                            output.extend(updated);
                        }
                    }
                }
            }
            at_start = false;
        } else {
            output.extend(fuse.process(depth[d].clone()));
            d += 1;
            at_start = false;
        }
    }
    output
}

fn order_events(mut events: Vec<StoredEvent>, base_latency: i64) -> Result<Vec<StoredEvent>> {
    let minimum_latency = events
        .iter()
        .map(|event| i128::from(event.local_ts) - i128::from(event.exch_ts))
        .min()
        .unwrap_or(0);
    if minimum_latency < 0 {
        let offset = -minimum_latency + i128::from(base_latency);
        for event in &mut events {
            event.local_ts = i64::try_from(i128::from(event.local_ts) + offset)
                .context("local timestamp correction exceeds i64 range")?;
        }
    }
    let mut exchange: Vec<_> = (0..events.len()).collect();
    let mut local = exchange.clone();
    exchange.sort_by_key(|&index| events[index].exch_ts);
    local.sort_by_key(|&index| events[index].local_ts);
    let (mut e, mut l) = (0, 0);
    let mut output = Vec::new();
    while e < exchange.len() || l < local.len() {
        let (index, flags) = if e < exchange.len() && l < local.len() && exchange[e] == local[l] {
            let index = exchange[e];
            e += 1;
            l += 1;
            (index, EXCH_EVENT | LOCAL_EVENT)
        } else if e < exchange.len()
            && (l == local.len()
                || (
                    events[exchange[e]].exch_ts,
                    events[exchange[e]].local_ts,
                    exchange[e],
                ) < (
                    events[local[l]].exch_ts,
                    events[local[l]].local_ts,
                    local[l],
                ))
        {
            let index = exchange[e];
            e += 1;
            (index, EXCH_EVENT)
        } else {
            let index = local[l];
            l += 1;
            (index, LOCAL_EVENT)
        };
        let mut event = events[index].clone();
        event.ev |= flags;
        output.push(event);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DEPTH_EVENT;

    fn event(ev: u64, px: i64, exch_ts: i64, local_ts: i64) -> StoredEvent {
        StoredEvent {
            ev,
            px,
            qty: 1,
            exch_ts,
            local_ts,
            order_id: 0,
            ival: 0,
            fval: 0.0,
        }
    }

    #[test]
    fn flushes_snapshot_at_end_of_file() {
        let input = [event(DEPTH_SNAPSHOT_EVENT | BUY_EVENT, 100, 1, 2)];
        let result = fuse_events(&input, &[], SnapshotMode::Process);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].ev, DEPTH_CLEAR_EVENT | BUY_EVENT);
        assert_eq!(result[1], input[0]);
        assert!(fuse_events(&input, &[], SnapshotMode::IgnoreSod).is_empty());
    }

    #[test]
    fn orders_both_clocks_without_losing_events_with_identical_timestamps() {
        let input = vec![
            event(DEPTH_EVENT | BUY_EVENT, 100, 20, 40),
            event(DEPTH_EVENT | SELL_EVENT, 101, 10, 50),
            event(DEPTH_EVENT | SELL_EVENT, 102, 10, 50),
        ];
        let result = order_events(input.clone(), 0).expect("timestamps should fit");
        for (flag, exchange) in [(EXCH_EVENT, true), (LOCAL_EVENT, false)] {
            let filtered: Vec<_> = result.iter().filter(|row| row.ev & flag != 0).collect();
            assert_eq!(filtered.len(), input.len());
            assert!(filtered.windows(2).all(|pair| if exchange {
                pair[0].exch_ts <= pair[1].exch_ts
            } else {
                pair[0].local_ts <= pair[1].local_ts
            }));
            let mut prices: Vec<_> = filtered.iter().map(|row| row.px).collect();
            prices.sort();
            assert_eq!(prices, vec![100, 101, 102]);
        }
    }

    #[test]
    fn latency_correction_is_checked_and_empty_input_is_valid() {
        assert!(
            order_events(vec![], 0)
                .expect("empty input should succeed")
                .is_empty()
        );
        let row = event(DEPTH_EVENT | BUY_EVENT, 1, 10, 1);
        assert_eq!(
            order_events(vec![row], 5).expect("latency correction should fit")[0].local_ts,
            15
        );
        assert!(order_events(vec![event(DEPTH_EVENT | BUY_EVENT, 1, i64::MAX, 0)], 1).is_err());
    }
}
