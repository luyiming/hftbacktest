use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use csv::{ByteRecord, Reader};
use flate2::read::MultiGzDecoder;

use crate::{
    backtest::data::{fixed::parse_fixed, format::StoredEvent},
    types::{
        BUY_EVENT, DEPTH_BBO_EVENT, DEPTH_EVENT, DEPTH_SNAPSHOT_EVENT, SELL_EVENT, TRADE_EVENT,
    },
};

#[derive(Clone, Copy, Debug)]
pub enum FeedKind {
    Trades,
    Depth,
    BookTicker,
}

/// Streams original CSV fields into exact fixed-point events, without a floating-point stage.
pub struct TardisReader {
    path: PathBuf,
    kind: FeedKind,
    reader: Reader<Box<dyn Read>>,
    headers: ByteRecord,
    record: ByteRecord,
    row: u64,
}

impl TardisReader {
    pub fn open(path: &Path, kind: FeedKind) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let input: Box<dyn Read> = if path.extension().is_some_and(|extension| extension == "gz") {
            Box::new(MultiGzDecoder::new(file))
        } else {
            Box::new(file)
        };
        Self::from_reader(path.to_owned(), kind, input)
    }

    pub fn from_reader(path: PathBuf, kind: FeedKind, input: Box<dyn Read>) -> Result<Self> {
        let mut reader = csv::ReaderBuilder::new().from_reader(input);
        let headers = reader
            .byte_headers()
            .with_context(|| format!("failed to read header of {}", path.display()))?
            .clone();
        let expected: &[&[u8]] = match kind {
            FeedKind::Trades => &[
                b"exchange",
                b"symbol",
                b"timestamp",
                b"local_timestamp",
                b"id",
                b"side",
                b"price",
                b"amount",
            ],
            FeedKind::Depth => &[
                b"exchange",
                b"symbol",
                b"timestamp",
                b"local_timestamp",
                b"is_snapshot",
                b"side",
                b"price",
                b"amount",
            ],
            FeedKind::BookTicker => &[
                b"exchange",
                b"symbol",
                b"timestamp",
                b"local_timestamp",
                b"ask_amount",
                b"ask_price",
                b"bid_price",
                b"bid_amount",
            ],
        };
        if !headers.iter().eq(expected.iter().copied()) {
            bail!("unexpected CSV header in {}: {:?}", path.display(), headers);
        }
        Ok(Self {
            path,
            kind,
            reader,
            headers,
            record: ByteRecord::new(),
            row: 1,
        })
    }

    /// A book ticker record yields ask then bid; other records yield one event.
    pub fn next_events(&mut self) -> Result<Option<Vec<StoredEvent>>> {
        let present = self
            .reader
            .read_byte_record(&mut self.record)
            .with_context(|| {
                format!(
                    "failed to read {} near record {}",
                    self.path.display(),
                    self.row + 1
                )
            })?;
        if !present {
            return Ok(None);
        }
        self.row += 1;
        let exch_ts = self.timestamp(2)?;
        let local_ts = self.timestamp(3)?;
        let make = |ev, px, qty| StoredEvent {
            ev,
            exch_ts,
            local_ts,
            px,
            qty,
            order_id: 0,
            ival: 0,
            fval: 0.0,
        };
        let events = match self.kind {
            FeedKind::BookTicker => vec![
                make(
                    DEPTH_BBO_EVENT | SELL_EVENT,
                    self.price(5)?,
                    self.quantity(4)?,
                ),
                make(
                    DEPTH_BBO_EVENT | BUY_EVENT,
                    self.price(6)?,
                    self.quantity(7)?,
                ),
            ],
            FeedKind::Trades | FeedKind::Depth => {
                let side = match &self.record[5] {
                    b"buy" | b"bid" => BUY_EVENT,
                    b"sell" | b"ask" => SELL_EVENT,
                    _ => bail!("{}: invalid side", self.location(5)),
                };
                let event = match self.kind {
                    FeedKind::Trades => TRADE_EVENT,
                    FeedKind::Depth => match &self.record[4] {
                        b"true" => DEPTH_SNAPSHOT_EVENT,
                        b"false" => DEPTH_EVENT,
                        _ => bail!("{}: invalid snapshot flag", self.location(4)),
                    },
                    FeedKind::BookTicker => unreachable!("book ticker is handled separately"),
                };
                vec![make(event | side, self.price(6)?, self.quantity(7)?)]
            }
        };
        Ok(Some(events))
    }

    fn location(&self, column: usize) -> String {
        format!(
            "{} record {} field {} value {:?}",
            self.path.display(),
            self.row,
            String::from_utf8_lossy(&self.headers[column]),
            String::from_utf8_lossy(&self.record[column])
        )
    }

    fn timestamp(&self, column: usize) -> Result<i64> {
        let result = std::str::from_utf8(&self.record[column])
            .with_context(|| self.location(column))?
            .parse::<i64>()
            .with_context(|| self.location(column))?;
        if result < 0 {
            bail!("{}: timestamp must be nonnegative", self.location(column));
        }
        result
            .checked_mul(1000)
            .with_context(|| format!("{}: nanosecond timestamp overflow", self.location(column)))
    }

    fn price(&self, column: usize) -> Result<i64> {
        let value = parse_fixed(&self.record[column]).with_context(|| self.location(column))?;
        if value <= 0 {
            bail!("{}: price must be positive", self.location(column));
        }
        Ok(value)
    }

    fn quantity(&self, column: usize) -> Result<i64> {
        let value = parse_fixed(&self.record[column]).with_context(|| self.location(column))?;
        if value < 0 {
            bail!("{}: quantity must be nonnegative", self.location(column));
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn trades(row: &str) -> TardisReader {
        let csv =
            format!("exchange,symbol,timestamp,local_timestamp,id,side,price,amount\n{row}\n");
        TardisReader::from_reader(
            PathBuf::from("trades.csv"),
            FeedKind::Trades,
            Box::new(Cursor::new(csv.into_bytes())),
        )
        .expect("valid header should load")
    }

    #[test]
    fn preserves_prices_beyond_f64_integer_precision() {
        let mut input =
            trades("binance,BTCUSDT,1,2,\"id,quoted\",buy,92233720368.54775807,0.00000001");
        assert_eq!(
            input.next_events().expect("valid row should parse"),
            Some(vec![StoredEvent {
                ev: TRADE_EVENT | BUY_EVENT,
                exch_ts: 1000,
                local_ts: 2000,
                px: i64::MAX,
                qty: 1,
                order_id: 0,
                ival: 0,
                fval: 0.0,
            }])
        );
        assert!(input.next_events().expect("EOF should parse").is_none());
    }

    #[test]
    fn reports_original_field_and_location() {
        let mut input = trades("binance,BTCUSDT,1,2,id,buy,1e-8,1");
        let error = input
            .next_events()
            .expect_err("scientific notation must fail");
        let message = format!("{error:#}");
        assert!(message.contains("trades.csv record 2 field price value \"1e-8\""));
        assert!(message.contains("plain decimal"));
    }

    #[test]
    fn checks_timestamp_scaling_overflow() {
        let mut input = trades("binance,BTCUSDT,9223372036854775807,2,id,buy,1,1");
        assert!(
            input
                .next_events()
                .expect_err("timestamp overflow must fail")
                .to_string()
                .contains("timestamp overflow")
        );
    }
}
