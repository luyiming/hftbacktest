use std::path::PathBuf;

use hftbacktest::backtest::data::convert::{ConvertRequest, SnapshotMode};
use pyo3::{exceptions::PyValueError, prelude::*};

#[pyfunction]
#[pyo3(signature = (trades_filename, depth_filename, output_filename, book_ticker_filename=None, snapshot_mode="process", base_latency=0))]
fn convert_fuse(
    py: Python<'_>,
    trades_filename: PathBuf,
    depth_filename: PathBuf,
    output_filename: PathBuf,
    book_ticker_filename: Option<PathBuf>,
    snapshot_mode: &str,
    base_latency: i64,
) -> PyResult<usize> {
    let snapshot_mode = match snapshot_mode {
        "process" => SnapshotMode::Process,
        "ignore" => SnapshotMode::Ignore,
        "ignore_sod" => SnapshotMode::IgnoreSod,
        _ => {
            return Err(PyValueError::new_err(
                "snapshot_mode must be process, ignore, or ignore_sod",
            ));
        },
    };
    py.detach(move || {
        hftbacktest::backtest::data::convert::convert_fuse(ConvertRequest {
            trades: &trades_filename,
            depth: &depth_filename,
            book_ticker: book_ticker_filename.as_deref(),
            output: &output_filename,
            snapshot_mode,
            base_latency,
        })
        .map_err(|error| PyValueError::new_err(format!("{error:#}")))
    })
}

#[pymodule]
fn _hftbacktest(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(convert_fuse, module)?)?;
    Ok(())
}
