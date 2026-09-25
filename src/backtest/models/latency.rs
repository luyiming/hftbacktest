use std::{
    fs::File,
    io::{self, Error as IoError},
    mem,
    path::Path,
    sync::Arc,
};

use npyz::DType;
use zip::ZipArchive;

use crate::{
    backtest::{
        BacktestError,
        data::{DataSource, Reader},
    },
    types::{OrderRequest, OrderUpdate},
};

/// Provides nonnegative order entry and response latencies.
pub trait LatencyModel {
    /// Returns a nonnegative order entry latency for the given timestamp and request.
    fn entry(&mut self, timestamp: i64, request: &OrderRequest) -> i64;

    /// Returns a nonnegative order response latency for the given timestamp and update.
    fn response(&mut self, timestamp: i64, update: &OrderUpdate) -> i64;
}

/// Provides constant order latency.
#[derive(Clone)]
pub struct ConstantLatency {
    entry_latency: i64,
    response_latency: i64,
}

impl ConstantLatency {
    /// Constructs an instance of `ConstantLatency`.
    ///
    /// `entry_latency` and `response_latency` should match the time unit of the data's timestamps.
    /// Using nanoseconds across all datasets is recommended.
    pub fn new(entry_latency: i64, response_latency: i64) -> Self {
        assert!(
            entry_latency >= 0,
            "order entry latency must be nonnegative"
        );
        assert!(
            response_latency >= 0,
            "order response latency must be nonnegative"
        );
        Self {
            entry_latency,
            response_latency,
        }
    }
}

impl LatencyModel for ConstantLatency {
    fn entry(&mut self, _timestamp: i64, _request: &OrderRequest) -> i64 {
        self.entry_latency
    }

    fn response(&mut self, _timestamp: i64, _update: &OrderUpdate) -> i64 {
        self.response_latency
    }
}

/// The historical order latency data
#[derive(Clone, Debug, npyz::Serialize, npyz::Deserialize)]
pub struct OrderLatencyRow {
    /// Timestamp at which the request occurs.
    pub req_ts: i64,
    /// Timestamp at which the exchange processes the request.
    pub exch_ts: i64,
    /// Timestamp at which the response is received.
    pub resp_ts: i64,
}

fn order_latency_dtype() -> DType {
    DType::parse("[('req_ts', '<i8'), ('exch_ts', '<i8'), ('resp_ts', '<i8')]")
        .expect("order latency dtype should be valid")
}

fn load_order_latency(path: &Path) -> io::Result<Vec<OrderLatencyRow>> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let mut file = archive.by_name("data.npy")?;
    let size = file.size();
    crate::backtest::data::npy::read_array(&mut file, size, &order_latency_dtype())
}

/// Provides order latency based on actual historical order latency data through interpolation.
///
/// However, if you don't have the actual order latency history, you can generate order latencies
/// artificially based on feed latency or using a custom model such as a regression model, which
/// incorporates factors like feed latency, trading volume, and the number of events.
///
/// Historical order latency data must contain positive exchange timestamps and nonnegative entry
/// and response latencies.
///
/// **Example**
/// ```no_run
/// use hftbacktest::backtest::{DataSource, models::IntpOrderLatency};
///
/// let latency_model = IntpOrderLatency::new(
///     vec![DataSource::File("latency_20240215.npz".into())],
///     0
/// );
/// ```
#[derive(Clone)]
pub struct IntpOrderLatency {
    entry_rn: usize,
    resp_rn: usize,
    reader: Reader<OrderLatencyRow>,
    data: Arc<Vec<OrderLatencyRow>>,
    next_data: Arc<Vec<OrderLatencyRow>>,
}

impl IntpOrderLatency {
    /// Constructs an `IntpOrderLatency` with options.
    pub fn build(
        data: Vec<DataSource<OrderLatencyRow>>,
        parallel_load: bool,
        latency_offset: i64,
    ) -> Result<Self, BacktestError> {
        let mut reader = if latency_offset == 0 {
            Reader::builder(load_order_latency)
                .parallel_load(parallel_load)
                .data(data)
                .build()?
        } else {
            Reader::builder(load_order_latency)
                .parallel_load(parallel_load)
                .data(data)
                .preprocess(move |data| adjust_order_latency(data, latency_offset))
                .build()?
        };
        let data = match reader.next_data() {
            Ok(data) => data,
            Err(BacktestError::EndOfData) => Arc::new(Vec::new()),
            Err(e) => return Err(e),
        };
        let next_data = match reader.next_data() {
            Ok(data) => data,
            Err(BacktestError::EndOfData) => Arc::new(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(Self {
            entry_rn: 0,
            resp_rn: 0,
            reader,
            data,
            next_data,
        })
    }

    /// Constructs an `IntpOrderLatency` with default options.
    pub fn new(data: Vec<DataSource<OrderLatencyRow>>, latency_offset: i64) -> Self {
        Self::build(data, true, latency_offset).unwrap()
    }

    fn intp(&self, x: i64, x1: i64, y1: i64, x2: i64, y2: i64) -> i64 {
        (((y2 - y1) as f64) / ((x2 - x1) as f64) * ((x - x1) as f64)) as i64 + y1
    }

    fn entry_latency(row: &OrderLatencyRow) -> i64 {
        assert!(
            row.exch_ts > 0,
            "exchange timestamps in order latency data must be positive"
        );
        let latency = row.exch_ts - row.req_ts;
        assert!(latency >= 0, "order entry latency must be nonnegative");
        latency
    }

    fn response_latency(row: &OrderLatencyRow) -> i64 {
        assert!(
            row.exch_ts > 0,
            "exchange timestamps in order latency data must be positive"
        );
        let latency = row.resp_ts - row.exch_ts;
        assert!(latency >= 0, "order response latency must be nonnegative");
        latency
    }

    fn next_data(&mut self) -> Result<bool, BacktestError> {
        if !self.next_data.is_empty() {
            let next_data = match self.reader.next_data() {
                Ok(data) => data,
                Err(BacktestError::EndOfData) => Arc::new(Vec::new()),
                Err(e) => return Err(e),
            };
            self.data = mem::replace(&mut self.next_data, next_data);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl LatencyModel for IntpOrderLatency {
    fn entry(&mut self, timestamp: i64, _request: &OrderRequest) -> i64 {
        let first_row = &self.data[0];
        if timestamp < first_row.req_ts {
            return Self::entry_latency(first_row);
        }

        loop {
            let row = &self.data[self.entry_rn];
            let next_row = if self.entry_rn + 1 < self.data.len() {
                &self.data[self.entry_rn + 1]
            } else if !self.next_data.is_empty() {
                &self.next_data[0]
            } else {
                let last_row = &self.data[self.data.len() - 1];
                return Self::entry_latency(last_row);
            };

            let req_local_timestamp = row.req_ts;
            let next_req_local_timestamp = next_row.req_ts;

            if row.req_ts <= timestamp && timestamp < next_row.req_ts {
                let lat1 = Self::entry_latency(row);
                let lat2 = Self::entry_latency(next_row);
                let latency = self.intp(
                    timestamp,
                    req_local_timestamp,
                    lat1,
                    next_req_local_timestamp,
                    lat2,
                );
                assert!(latency >= 0, "order entry latency must be nonnegative");
                return latency;
            } else if self.entry_rn == self.data.len() - 1 {
                if self.next_data().unwrap() {
                    self.entry_rn = 0;
                }
            } else {
                self.entry_rn += 1;
            }
        }
    }

    fn response(&mut self, timestamp: i64, _update: &OrderUpdate) -> i64 {
        let first_row = &self.data[0];
        if timestamp < first_row.exch_ts {
            return Self::response_latency(first_row);
        }

        loop {
            let row = &self.data[self.resp_rn];
            let next_row = if self.resp_rn + 1 < self.data.len() {
                &self.data[self.resp_rn + 1]
            } else if !self.next_data.is_empty() {
                &self.next_data[0]
            } else {
                let last_row = &self.data[self.data.len() - 1];
                return Self::response_latency(last_row);
            };

            let exch_timestamp = row.exch_ts;
            let next_exch_timestamp = next_row.exch_ts;
            if exch_timestamp <= timestamp && timestamp < next_exch_timestamp {
                let lat1 = Self::response_latency(row);
                let lat2 = Self::response_latency(next_row);

                let lat = self.intp(timestamp, exch_timestamp, lat1, next_exch_timestamp, lat2);
                assert!(lat >= 0);
                return lat;
            } else if self.resp_rn == self.data.len() - 1 {
                if self.next_data().unwrap() {
                    self.resp_rn = 0;
                }
            } else {
                self.resp_rn += 1;
            }
        }
    }
}

fn adjust_order_latency(data: &mut [OrderLatencyRow], latency_offset: i64) -> io::Result<()> {
    let response_offset = latency_offset
        .checked_mul(2)
        .ok_or_else(|| IoError::new(io::ErrorKind::InvalidData, "latency offset overflow"))?;
    for row in data {
        row.exch_ts = row.exch_ts.checked_add(latency_offset).ok_or_else(|| {
            IoError::new(
                io::ErrorKind::InvalidData,
                "exchange timestamp offset overflow",
            )
        })?;
        row.resp_ts = row.resp_ts.checked_add(response_offset).ok_or_else(|| {
            IoError::new(
                io::ErrorKind::InvalidData,
                "response timestamp offset overflow",
            )
        })?;
    }
    Ok(())
}
