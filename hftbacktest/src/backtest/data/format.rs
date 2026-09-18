use std::{
    fs::File,
    io::{self, Read, Seek},
    path::Path,
};

use hftbacktest_derive::NpyDTyped;
use tempfile::NamedTempFile;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::backtest::data::{
    Data, POD,
    fixed::DATA_SCALE,
    npy::{read_npy, write_npy},
};

/// On-disk event. Prices and quantities are signed integers at DATA_SCALE.
#[repr(C, align(64))]
#[derive(Clone, Debug, PartialEq, NpyDTyped)]
pub struct StoredEvent {
    pub ev: u64,
    pub exch_ts: i64,
    pub local_ts: i64,
    pub px: i64,
    pub qty: i64,
    pub order_id: u64,
    pub ival: i64,
    pub fval: f64,
}

// All fields are primitive values and the 64-byte record has no padding.
unsafe impl POD for StoredEvent {}

#[repr(C)]
#[derive(Clone, Debug, PartialEq, Eq, NpyDTyped)]
pub struct MarketDataMetadata {
    pub format_version: u32,
    pub price_scale: u32,
    pub size_scale: u32,
}

unsafe impl POD for MarketDataMetadata {}

impl MarketDataMetadata {
    pub const CURRENT: Self = Self {
        format_version: 1,
        price_scale: DATA_SCALE,
        size_scale: DATA_SCALE,
    };
}

fn read_array<R: Read + Seek, D: crate::backtest::data::NpyDTyped + Clone>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> io::Result<Data<D>> {
    let mut file = archive.by_name(name)?;
    let size = usize::try_from(file.size()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "array exceeds addressable size")
    })?;
    read_npy(&mut file, size)
}

pub fn read_market_data<R: Read + Seek>(reader: R) -> io::Result<Data<StoredEvent>> {
    if !cfg!(target_endian = "little") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "market data requires a little-endian host",
        ));
    }
    let mut archive = ZipArchive::new(reader)?;
    let metadata: Data<MarketDataMetadata> = read_array(&mut archive, "metadata.npy")?;
    if metadata.len() != 1 || metadata[0] != MarketDataMetadata::CURRENT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported market data metadata",
        ));
    }
    read_array(&mut archive, "data.npy")
}

pub fn read_market_data_file(path: &Path) -> io::Result<Data<StoredEvent>> {
    read_market_data(File::open(path)?)
}

/// Publishes a complete NPZ atomically; errors leave any existing destination intact.
pub fn write_market_data_file(path: &Path, events: &[StoredEvent]) -> io::Result<()> {
    if !cfg!(target_endian = "little") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "market data requires a little-endian host",
        ));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    {
        let mut archive = ZipWriter::new(temporary.as_file_mut());
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        archive.start_file("metadata.npy", options)?;
        write_npy(&mut archive, &[MarketDataMetadata::CURRENT])?;
        archive.start_file("data.npy", options)?;
        write_npy(&mut archive, events)?;
        archive.finish()?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_truncated_payload_and_dtype_mismatch() {
        let events = [StoredEvent {
            ev: 1,
            exch_ts: 2,
            local_ts: 3,
            px: 4,
            qty: 5,
            order_id: 0,
            ival: 0,
            fval: 0.0,
        }];
        let mut bytes = Vec::new();
        write_npy(&mut bytes, &events).expect("valid events should serialize");
        let size = bytes.len();
        assert!(read_npy::<_, StoredEvent>(&mut Cursor::new(&bytes[..size - 1]), size).is_err());
        assert!(
            read_npy::<_, StoredEvent>(&mut Cursor::new(&bytes[..size - 1]), size - 1).is_err()
        );
        let dtype = bytes
            .windows(13)
            .position(|window| window == b"('px', '<i8')")
            .expect("price dtype should be in header");
        bytes[dtype + 9] = b'f';
        assert!(read_npy::<_, StoredEvent>(&mut Cursor::new(bytes), size).is_err());
    }
    use std::io::Cursor;

    #[test]
    fn exact_event_roundtrip() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("events.npz");
        let events = [StoredEvent {
            ev: 1,
            exch_ts: 2,
            local_ts: 3,
            px: i64::MAX,
            qty: 1,
            order_id: 0,
            ival: 0,
            fval: 0.0,
        }];
        write_market_data_file(&path, &events).expect("valid events should serialize");
        let loaded = read_market_data_file(&path).expect("valid archive should load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], events[0]);
    }

    #[test]
    fn empty_array_roundtrip() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("empty.npz");
        write_market_data_file(&path, &[]).expect("empty array should serialize");
        assert!(
            read_market_data_file(&path)
                .expect("empty array should load")
                .is_empty()
        );
    }

    #[test]
    fn rejects_missing_or_invalid_metadata() {
        for metadata in [
            None,
            Some(MarketDataMetadata {
                format_version: 1,
                price_scale: 9,
                size_scale: 8,
            }),
            Some(MarketDataMetadata {
                format_version: 2,
                price_scale: 8,
                size_scale: 8,
            }),
        ] {
            let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
            if let Some(metadata) = metadata {
                writer
                    .start_file("metadata.npy", SimpleFileOptions::default())
                    .expect("entry should open");
                write_npy(&mut writer, &[metadata]).expect("metadata should serialize");
            }
            writer
                .start_file("data.npy", SimpleFileOptions::default())
                .expect("entry should open");
            write_npy::<_, StoredEvent>(&mut writer, &[]).expect("empty array should serialize");
            let bytes = writer.finish().expect("archive should finish").into_inner();
            assert!(read_market_data(Cursor::new(bytes)).is_err());
        }
    }
}
