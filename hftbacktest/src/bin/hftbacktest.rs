use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use hftbacktest::backtest::data::convert::{ConvertRequest, EodOutput, SnapshotMode, convert_fuse};

#[derive(Parser)]
#[command(about = "HftBacktest data tools")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Convert Tardis trades, depth, and optional book ticker into fused market data.
    ConvertFuse(ConvertFuseArgs),
}

#[derive(clap::Args)]
struct ConvertFuseArgs {
    #[arg(long)]
    trades_filename: PathBuf,
    #[arg(long)]
    depth_filename: PathBuf,
    #[arg(long)]
    output_filename: PathBuf,
    #[arg(long)]
    book_ticker_filename: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = SnapshotModeArg::Process)]
    snapshot_mode: SnapshotModeArg,
    #[arg(long, default_value_t = 0)]
    base_latency: i64,
    #[arg(long)]
    initial_snapshot_filename: Option<PathBuf>,
    #[arg(long, requires = "eod_timestamp")]
    eod_filename: Option<PathBuf>,
    #[arg(long, requires = "eod_filename")]
    eod_timestamp: Option<i64>,
}

#[derive(Clone, Copy, ValueEnum)]
enum SnapshotModeArg {
    Process,
    Ignore,
    IgnoreSod,
}

impl From<SnapshotModeArg> for SnapshotMode {
    fn from(value: SnapshotModeArg) -> Self {
        match value {
            SnapshotModeArg::Process => Self::Process,
            SnapshotModeArg::Ignore => Self::Ignore,
            SnapshotModeArg::IgnoreSod => Self::IgnoreSod,
        }
    }
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::ConvertFuse(args) => {
            convert_fuse(ConvertRequest {
                trades: &args.trades_filename,
                depth: &args.depth_filename,
                book_ticker: args.book_ticker_filename.as_deref(),
                output: &args.output_filename,
                snapshot_mode: args.snapshot_mode.into(),
                base_latency: args.base_latency,
                initial_snapshot: args.initial_snapshot_filename.as_deref(),
                eod_output: args
                    .eod_filename
                    .as_deref()
                    .zip(args.eod_timestamp)
                    .map(|(path, timestamp)| EodOutput { path, timestamp }),
            })
            .with_context(|| format!("failed to convert {}", args.output_filename.display()))?;
        }
    }
    Ok(())
}
