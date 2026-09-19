use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Unbounded},
};

use crate::{
    backtest::data::format::StoredEvent,
    types::{
        BUY_EVENT, DEPTH_BBO_EVENT, DEPTH_CLEAR_EVENT, DEPTH_EVENT, DEPTH_SNAPSHOT_EVENT,
        SELL_EVENT,
    },
};

#[derive(Clone, Debug)]
struct Level {
    qty: i64,
    timestamp: i64,
}

#[derive(Default)]
struct BookSide {
    levels: BTreeMap<i64, Level>,
    best_timestamp: i64,
}

/// Fuses depth and BBO using exact observed price levels, independent of market tick rules.
#[derive(Default)]
pub struct FixedFuse {
    bids: BookSide,
    asks: BookSide,
}

impl FixedFuse {
    pub fn process(&mut self, mut event: StoredEvent) -> Vec<StoredEvent> {
        let kind = event.ev & 0xff;
        let buy = event.ev & BUY_EVENT != 0;
        if kind == DEPTH_CLEAR_EVENT {
            if buy {
                self.bids.levels.retain(|price, _| *price < event.px);
                self.bids.best_timestamp = event.exch_ts;
            } else if event.ev & SELL_EVENT != 0 {
                self.asks.levels.retain(|price, _| *price > event.px);
                self.asks.best_timestamp = event.exch_ts;
            } else {
                self.bids.levels.clear();
                self.asks.levels.clear();
                self.bids.best_timestamp = event.exch_ts;
                self.asks.best_timestamp = event.exch_ts;
            }
            return vec![event];
        }
        assert!(matches!(
            kind,
            DEPTH_EVENT | DEPTH_SNAPSHOT_EVENT | DEPTH_BBO_EVENT
        ));
        let bbo = kind == DEPTH_BBO_EVENT;
        let (own, opposite) = if buy {
            (&mut self.bids, &mut self.asks)
        } else {
            (&mut self.asks, &mut self.bids)
        };
        let best = if buy {
            own.levels.last_key_value()
        } else {
            own.levels.first_key_value()
        }
        .map(|(&px, _)| px);
        let opposite_best = if buy {
            opposite.levels.first_key_value()
        } else {
            opposite.levels.last_key_value()
        }
        .map(|(&px, _)| px);
        let touches_best = best.is_none_or(|px| if buy { event.px >= px } else { event.px <= px });
        let crosses =
            opposite_best.is_some_and(|px| if buy { event.px >= px } else { event.px <= px });
        if ((bbo || touches_best) && event.exch_ts < own.best_timestamp)
            || (crosses && event.exch_ts < opposite.best_timestamp)
            || own
                .levels
                .get(&event.px)
                .is_some_and(|level| event.exch_ts < level.timestamp)
        {
            return vec![];
        }
        if bbo {
            event.ev = (event.ev & !0xff) | DEPTH_EVENT;
            if best == Some(event.px)
                && own
                    .levels
                    .get(&event.px)
                    .is_some_and(|level| level.qty == event.qty)
            {
                own.best_timestamp = event.exch_ts;
                return vec![];
            }
        }
        let existed = own.levels.contains_key(&event.px);
        if event.qty == 0 {
            own.levels.remove(&event.px);
        } else {
            own.levels.insert(
                event.px,
                Level {
                    qty: event.qty,
                    timestamp: event.exch_ts,
                },
            );
        }
        let mut output = if existed || event.qty > 0 {
            vec![event.clone()]
        } else {
            vec![]
        };
        if bbo || touches_best {
            own.best_timestamp = event.exch_ts;
        }
        if event.qty > 0 && crosses {
            let prices: Vec<_> = if buy {
                opposite
                    .levels
                    .range(..=event.px)
                    .map(|(&px, _)| px)
                    .collect()
            } else {
                opposite
                    .levels
                    .range(event.px..)
                    .map(|(&px, _)| px)
                    .collect()
            };
            Self::remove_levels(opposite, prices, !buy, &event, &mut output);
            opposite.best_timestamp = event.exch_ts;
        }
        if bbo {
            let prices: Vec<_> = if buy {
                own.levels
                    .range((Excluded(event.px), Unbounded))
                    .map(|(&px, _)| px)
                    .collect()
            } else {
                own.levels.range(..event.px).map(|(&px, _)| px).collect()
            };
            Self::remove_levels(own, prices, buy, &event, &mut output);
        }
        output
    }

    fn remove_levels(
        book: &mut BookSide,
        prices: Vec<i64>,
        buy: bool,
        source: &StoredEvent,
        output: &mut Vec<StoredEvent>,
    ) {
        for px in prices {
            book.levels.remove(&px);
            output.push(StoredEvent {
                ev: DEPTH_EVENT | if buy { BUY_EVENT } else { SELL_EVENT },
                px,
                qty: 0,
                ..source.clone()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u64, px: i64, qty: i64, timestamp: i64) -> StoredEvent {
        StoredEvent {
            ev: kind,
            px,
            qty,
            exch_ts: timestamp,
            local_ts: timestamp + 1,
        }
    }

    #[test]
    fn crosses_sparse_levels_without_rounding_or_scanning_gaps() {
        let mut fuse = FixedFuse::default();
        fuse.process(event(DEPTH_EVENT | SELL_EVENT, 1, 1, 1));
        fuse.process(event(DEPTH_EVENT | SELL_EVENT, i64::MAX, 1, 1));
        let input = event(DEPTH_EVENT | BUY_EVENT, i64::MAX - 1, 1, 2);
        assert_eq!(
            fuse.process(input.clone()),
            vec![input, event(DEPTH_EVENT | SELL_EVENT, 1, 0, 2)]
        );
        assert_eq!(fuse.asks.levels.len(), 1);
    }

    #[test]
    fn ticker_backoff_removes_only_better_levels_and_rejects_stale_updates() {
        let mut fuse = FixedFuse::default();
        fuse.process(event(DEPTH_EVENT | BUY_EVENT, 100, 1, 1));
        fuse.process(event(DEPTH_EVENT | BUY_EVENT, 90, 1, 1));
        assert_eq!(
            fuse.process(event(DEPTH_BBO_EVENT | BUY_EVENT, 95, 2, 3)),
            vec![
                event(DEPTH_EVENT | BUY_EVENT, 95, 2, 3),
                event(DEPTH_EVENT | BUY_EVENT, 100, 0, 3),
            ]
        );
        assert!(
            fuse.process(event(DEPTH_EVENT | BUY_EVENT, 100, 1, 2))
                .is_empty()
        );
        assert_eq!(
            fuse.bids.levels.keys().copied().collect::<Vec<_>>(),
            vec![90, 95]
        );
    }

    #[test]
    fn snapshot_clear_keeps_adjacent_outside_level() {
        let mut fuse = FixedFuse::default();
        fuse.process(event(DEPTH_EVENT | BUY_EVENT, 99, 1, 1));
        fuse.process(event(DEPTH_EVENT | BUY_EVENT, 100, 1, 1));
        fuse.process(event(DEPTH_CLEAR_EVENT | BUY_EVENT, 100, 0, 2));
        assert_eq!(
            fuse.bids.levels.keys().copied().collect::<Vec<_>>(),
            vec![99]
        );
    }
}
