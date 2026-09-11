use std::{
    fs::{self, File},
    path::PathBuf,
};

use zip::{ZipWriter, write::SimpleFileOptions};

use super::*;
use crate::backtest::{BacktestError, data::write_npy};

struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new() -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("hft-snapshot-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn write(&self, name: &str, feed: &[Event]) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.0.join(name);
        let mut file = File::create(&path)?;
        if name.ends_with(".npz") {
            let mut zip = ZipWriter::new(file);
            zip.start_file("data.npy", SimpleFileOptions::default())?;
            write_npy(&mut zip, feed)?;
            zip.finish()?;
        } else {
            write_npy(&mut file, feed)?;
        }
        Ok(path)
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("owned snapshot test directory should be removable");
    }
}

#[test]
fn restore_crosses_files_without_reloading_the_saved_prefix() -> TestResult {
    for extension in ["npy", "npz"] {
        let dir = FixtureDir::new()?;
        let feed = events();
        let first = dir.write(&format!("first.{extension}"), &feed[..6])?;
        let second = dir.write(&format!("second.{extension}"), &feed[6..])?;
        let sources = [&first, &second]
            .into_iter()
            .map(|path| DataSource::File(path.display().to_string()))
            .collect();
        let mut original = Backtest::builder().add_asset(asset(sources, 0)?).build()?;
        original.elapse(0)?;
        original.submit_buy_order(0, 1, 100.0, 5.0, TimeInForce::GTX, OrdType::Limit, true)?;
        advance(&mut original, 5)?;
        let snapshot = original.snapshot()?;
        let mut early_branch = snapshot.restore()?;
        fs::rename(&first, dir.0.join("saved-prefix"))?;
        original.goto_end()?;
        let expected = view(&original);
        drop(original);
        early_branch.goto_end()?;
        assert_eq!(view(&early_branch), expected);
        drop(early_branch);
        for _ in 0..4 {
            let mut restored = snapshot.restore()?;
            restored.goto_end()?;
            assert_eq!(view(&restored), expected);
        }
    }
    Ok(())
}

#[test]
fn prefetch_failure_is_reported_by_each_branch_at_the_affected_file() -> TestResult {
    let dir = FixtureDir::new()?;
    let first = dir.write("first.npy", &events()[..6])?;
    let missing = dir.0.join("missing.npy");
    let sources = [first, missing]
        .into_iter()
        .map(|path| DataSource::File(path.display().to_string()))
        .collect();
    let mut original = Backtest::builder().add_asset(asset(sources, 0)?).build()?;
    original.elapse(0)?;
    let snapshot = original.snapshot()?;
    let mut restored = snapshot.restore()?;
    for hbt in [&mut original, &mut restored] {
        assert!(matches!(hbt.goto_end(), Err(BacktestError::DataError(_))));
    }
    assert!(matches!(
        snapshot.restore()?.goto_end(),
        Err(BacktestError::DataError(_))
    ));
    Ok(())
}

#[test]
fn unsupported_sources_return_errors_to_repeated_restores() -> TestResult {
    let mut original = Backtest::builder()
        .add_asset(asset(
            vec![DataSource::File("snapshot-test.invalid".into())],
            0,
        )?)
        .build()?;
    let snapshot = original.snapshot()?;
    assert!(matches!(
        original.elapse(0),
        Err(BacktestError::DataError(_))
    ));
    for _ in 0..3 {
        assert!(matches!(
            snapshot.restore()?.elapse(0),
            Err(BacktestError::DataError(_))
        ));
    }
    Ok(())
}

#[test]
fn preprocessing_is_not_repeated_after_snapshot_restore() -> TestResult {
    let mut original = Backtest::builder()
        .add_asset(
            L2AssetBuilder::new()
                .data(vec![DataSource::Data(Data::from_data(&events()))])
                .latency_offset(2)
                .latency_model(ConstantLatency::new(0, 0))
                .asset_type(LinearAsset::new(1.0))
                .fee_model(TradingValueFeeModel::new(CommonFees::new(0.00009, 0.00027)))
                .queue_model(RiskAdverseQueueModel::new())
                .last_trades_capacity(8)
                .depth(|| BTreeMarketDepth::new(1.0, 1.0))
                .build_snapshotable()?,
        )
        .build()?;
    original.elapse(5)?;
    let mut restored = original.snapshot()?.restore()?;
    original.goto_end()?;
    restored.goto_end()?;
    assert_eq!(view(&original), view(&restored));
    assert_eq!(restored.last_trades(0)[0].local_ts, 12);
    Ok(())
}
