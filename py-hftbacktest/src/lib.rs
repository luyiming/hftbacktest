use std::path::PathBuf;

use hftbacktest::backtest::data::convert::{ConvertRequest, EodOutput, SnapshotMode};
use pyo3::{exceptions::PyValueError, prelude::*};

#[pyfunction]
#[pyo3(signature = (trades_filename, depth_filename, output_filename, book_ticker_filename=None, snapshot_mode="process", base_latency=0, initial_snapshot_filename=None, eod_filename=None, eod_timestamp=None))]
fn convert_fuse(
    py: Python<'_>,
    trades_filename: PathBuf,
    depth_filename: PathBuf,
    output_filename: PathBuf,
    book_ticker_filename: Option<PathBuf>,
    snapshot_mode: &str,
    base_latency: i64,
    initial_snapshot_filename: Option<PathBuf>,
    eod_filename: Option<PathBuf>,
    eod_timestamp: Option<i64>,
) -> PyResult<usize> {
    if eod_filename.is_some() != eod_timestamp.is_some() {
        return Err(PyValueError::new_err(
            "eod_filename and eod_timestamp must be provided together",
        ));
    }
    let snapshot_mode = match snapshot_mode {
        "process" => SnapshotMode::Process,
        "ignore" => SnapshotMode::Ignore,
        "ignore_sod" => SnapshotMode::IgnoreSod,
        _ => {
            return Err(PyValueError::new_err(
                "snapshot_mode must be process, ignore, or ignore_sod",
            ));
        }
    };
    py.detach(move || {
        hftbacktest::backtest::data::convert::convert_fuse(ConvertRequest {
            trades: &trades_filename,
            depth: &depth_filename,
            book_ticker: book_ticker_filename.as_deref(),
            output: &output_filename,
            snapshot_mode,
            base_latency,
            initial_snapshot: initial_snapshot_filename.as_deref(),
            eod_output: eod_filename
                .as_deref()
                .zip(eod_timestamp)
                .map(|(path, timestamp)| EodOutput { path, timestamp }),
        })
        .map_err(|error| PyValueError::new_err(format!("{error:#}")))
    })
}

#[pymodule]
fn _hftbacktest(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(convert_fuse, module)?)?;
    Ok(())
}
