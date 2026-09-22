use std::{
    fs::File,
    io::{BufWriter, Error, Write},
    path::Path,
};

use npyz::DType;
use rust_decimal::prelude::ToPrimitive;
use tempfile::NamedTempFile;
use zip::{ZipWriter, write::SimpleFileOptions};

use crate::{
    backtest::data::npy::write_array,
    depth::MarketDepth,
    types::{Bot, Recorder},
};

#[derive(npyz::Serialize, npyz::Deserialize)]
struct Record {
    timestamp: i64,
    price: f64,
    position: f64,
    balance: f64,
    fee: f64,
    num_trades: i64,
    trading_volume: f64,
    trading_value: f64,
}

fn record_dtype() -> DType {
    DType::parse(
        "[('timestamp', '<i8'), ('price', '<f8'), ('position', '<f8'), ('balance', '<f8'), ('fee', '<f8'), ('num_trades', '<i8'), ('trading_volume', '<f8'), ('trading_value', '<f8')]",
    )
    .expect("recorder dtype should be valid")
}

/// Provides recording of the backtesting strategy's state values, which are needed to compute
/// performance metrics.
pub struct BacktestRecorder {
    values: Vec<Vec<Record>>,
}

impl Recorder for BacktestRecorder {
    type Error = Error;

    fn record<MD, I>(&mut self, hbt: &I) -> Result<(), Self::Error>
    where
        MD: MarketDepth,
        I: Bot<MD>,
    {
        let timestamp = hbt.current_timestamp();
        for asset_no in 0..hbt.num_assets() {
            let depth = hbt.depth(asset_no);
            let mid_price = match (depth.best_bid(), depth.best_ask()) {
                (Some(bid), Some(ask)) => ((bid + ask) / rust_decimal::Decimal::TWO)
                    .to_f64()
                    .expect("mid price should fit f64"),
                _ => f64::NAN,
            };
            let state_values = hbt.state_values(asset_no);
            let values = self
                .values
                .get_mut(asset_no)
                .expect("recorder should contain one buffer per asset");
            values.push(Record {
                timestamp,
                price: mid_price,
                balance: state_values
                    .balance
                    .to_f64()
                    .expect("balance should fit f64"),
                position: state_values
                    .position
                    .to_f64()
                    .expect("position should fit f64"),
                fee: state_values.fee.to_f64().expect("fee should fit f64"),
                trading_volume: state_values
                    .trading_volume
                    .to_f64()
                    .expect("trading volume should fit f64"),
                trading_value: state_values
                    .trading_value
                    .to_f64()
                    .expect("trading value should fit f64"),
                num_trades: state_values.num_trades,
            });
        }
        Ok(())
    }
}

impl BacktestRecorder {
    /// Constructs an instance of `BacktestRecorder`.
    pub fn new<I, MD>(hbt: &I) -> Self
    where
        MD: MarketDepth,
        I: Bot<MD>,
    {
        Self {
            values: {
                let mut vec = Vec::with_capacity(hbt.num_assets());
                for _ in 0..hbt.num_assets() {
                    vec.push(Vec::new());
                }
                vec
            },
        }
    }

    /// Saves record data into a CSV file at the specified path. It creates a separate CSV file for
    /// each asset, with the filename `{prefix}_{asset_no}.csv`.
    /// The columns are `timestamp`, `mid`, `balance`, `position`, `fee`, `trade_num`,
    /// `trade_amount`, `trade_qty`.
    pub fn to_csv<Prefix, P>(&self, prefix: Prefix, path: P) -> Result<(), Error>
    where
        Prefix: AsRef<str>,
        P: AsRef<Path>,
    {
        let prefix = prefix.as_ref();
        for (asset_no, values) in self.values.iter().enumerate() {
            let file_path = path.as_ref().join(format!("{prefix}{asset_no}.csv"));
            let mut file = BufWriter::new(File::create(file_path)?);
            writeln!(
                file,
                "timestamp,balance,position,fee,trading_volume,trading_value,num_trades,price",
            )?;
            for Record {
                timestamp,
                balance,
                position,
                fee,
                trading_volume,
                trading_value,
                num_trades,
                price: mid_price,
            } in values
            {
                writeln!(
                    file,
                    "{timestamp},{balance},{position},{fee},{trading_volume},{trading_value},{num_trades},{mid_price}"
                )?;
            }
        }
        Ok(())
    }

    pub fn to_npz<P>(&self, path: P) -> Result<(), Error>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = NamedTempFile::new_in(parent)?;
        let mut zip = ZipWriter::new(temporary.as_file_mut());

        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::DEFLATE)
            .compression_level(Some(9));

        for (asset_no, values) in self.values.iter().enumerate() {
            zip.start_file(format!("{asset_no}.npy"), options)?;
            write_array(&mut zip, values, record_dtype())?;
        }

        zip.finish()?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}
