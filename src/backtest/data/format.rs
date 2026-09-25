use std::{
    fs::File,
    io::{self, BufWriter, Read, Seek, Write},
    path::Path,
};

use npyz::DType;
use rust_decimal::Decimal;
use tempfile::NamedTempFile;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::{
    backtest::data::{
        fixed::DATA_SCALE,
        npy::{read_array, write_array},
    },
    types::Event,
};

/// On-disk event. Prices and quantities are signed integers at [`DATA_SCALE`].
#[derive(Clone, Debug, PartialEq, Eq, npyz::Serialize, npyz::Deserialize)]
pub struct StoredEvent {
    pub ev: u64,
    pub exch_ts: i64,
    pub local_ts: i64,
    pub px: i64,
    pub qty: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, npyz::Serialize, npyz::Deserialize)]
struct MarketDataMetadata {
    price_scale: u32,
    size_scale: u32,
}

impl MarketDataMetadata {
    const CURRENT: Self = Self {
        price_scale: DATA_SCALE,
        size_scale: DATA_SCALE,
    };
}

fn stored_event_dtype() -> DType {
    DType::parse(
        "[('ev', '<u8'), ('exch_ts', '<i8'), ('local_ts', '<i8'), ('px', '<i8'), ('qty', '<i8')]",
    )
    .expect("stored event dtype should be valid")
}

fn metadata_dtype() -> DType {
    DType::parse("[('price_scale', '<u4'), ('size_scale', '<u4')]")
        .expect("market data metadata dtype should be valid")
}

fn read_zip_array<R, T>(
    archive: &mut ZipArchive<R>,
    name: &str,
    dtype: &DType,
) -> io::Result<Vec<T>>
where
    R: Read + Seek,
    T: npyz::Deserialize,
{
    let mut file = archive.by_name(name)?;
    let size = file.size();
    read_array(&mut file, size, dtype)
}

pub(crate) fn read_stored_events<R: Read + Seek>(reader: R) -> io::Result<Vec<StoredEvent>> {
    let mut archive = ZipArchive::new(reader)?;
    let metadata: Vec<MarketDataMetadata> =
        read_zip_array(&mut archive, "metadata.npy", &metadata_dtype())?;
    if metadata.as_slice() != [MarketDataMetadata::CURRENT] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported market data scale metadata",
        ));
    }
    read_zip_array(&mut archive, "data.npy", &stored_event_dtype())
}

pub fn read_market_data<R: Read + Seek>(reader: R) -> io::Result<Vec<Event>> {
    read_stored_events(reader).map(|events| {
        events
            .into_iter()
            .map(|event| Event {
                ev: event.ev,
                exch_ts: event.exch_ts,
                local_ts: event.local_ts,
                px: Decimal::from_i128_with_scale(i128::from(event.px), DATA_SCALE),
                qty: Decimal::from_i128_with_scale(i128::from(event.qty), DATA_SCALE),
            })
            .collect()
    })
}

pub fn read_market_data_file(path: &Path) -> io::Result<Vec<Event>> {
    read_market_data(File::open(path)?)
}

/// Publishes a complete NPZ atomically; errors leave any existing destination intact.
pub fn write_market_data_file(path: &Path, events: &[StoredEvent]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    {
        let mut archive = ZipWriter::new(temporary.as_file_mut());
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        archive.start_file("metadata.npy", options)?;
        write_array(
            &mut archive,
            &[MarketDataMetadata::CURRENT],
            metadata_dtype(),
        )?;
        archive.start_file("data.npy", options.large_file(true))?;
        {
            // npyz writes each field separately; batch those writes before compression.
            let mut buffered = BufWriter::with_capacity(1024 * 1024, &mut archive);
            write_array(&mut buffered, events, stored_event_dtype())?;
            buffered.flush()?;
        }
        archive.finish()?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, SeekFrom};

    use super::*;

    fn stored_event() -> StoredEvent {
        StoredEvent {
            ev: 1,
            exch_ts: 2,
            local_ts: 3,
            px: i64::MAX,
            qty: 1,
        }
    }

    #[test]
    fn exact_event_roundtrip() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("events.npz");
        write_market_data_file(&path, &[stored_event()]).expect("valid events should serialize");
        let loaded = read_market_data_file(&path).expect("valid archive should load");
        assert_eq!(
            loaded,
            [Event {
                ev: 1,
                exch_ts: 2,
                local_ts: 3,
                px: Decimal::from_i128_with_scale(i128::from(i64::MAX), DATA_SCALE),
                qty: Decimal::from_i128_with_scale(1, DATA_SCALE),
            }]
        );
    }

    #[test]
    fn data_entry_reserves_zip64_sizes() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("events.npz");
        write_market_data_file(&path, &[stored_event()]).expect("valid events should serialize");
        let mut archive = ZipArchive::new(File::open(&path).expect("archive should open"))
            .expect("archive should load");
        let header_start = archive
            .by_name("data.npy")
            .expect("data entry should exist")
            .header_start();
        let mut file = File::open(&path).expect("archive should reopen");
        file.seek(SeekFrom::Start(header_start + 18))
            .expect("local header should be readable");
        let mut sizes = [0_u8; 8];
        file.read_exact(&mut sizes)
            .expect("local header should contain both sizes");
        assert_eq!(sizes, [u8::MAX; 8]);
    }

    #[test]
    fn roundtrips_data_larger_than_write_buffer() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("large.npz");
        let events: Vec<_> = (0..30_000)
            .map(|index| StoredEvent {
                ev: 1,
                exch_ts: index,
                local_ts: index + 1,
                px: index + 2,
                qty: index + 3,
            })
            .collect();
        write_market_data_file(&path, &events).expect("valid events should serialize");
        let loaded = read_stored_events(File::open(&path).expect("archive should open"))
            .expect("archive should load");
        assert_eq!(loaded, events);
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
                price_scale: 9,
                size_scale: 8,
            }),
        ] {
            let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
            if let Some(metadata) = metadata {
                writer
                    .start_file("metadata.npy", SimpleFileOptions::default())
                    .expect("entry should open");
                write_array(&mut writer, &[metadata], metadata_dtype())
                    .expect("metadata should serialize");
            }
            writer
                .start_file("data.npy", SimpleFileOptions::default())
                .expect("entry should open");
            write_array::<_, StoredEvent>(&mut writer, &[], stored_event_dtype())
                .expect("empty array should serialize");
            let bytes = writer.finish().expect("archive should finish").into_inner();
            assert!(read_market_data(Cursor::new(bytes)).is_err());
        }
    }

    #[test]
    fn rejects_trailing_payload_and_dtype_mismatch() {
        let mut bytes = Vec::new();
        write_array(&mut bytes, &[stored_event()], stored_event_dtype())
            .expect("valid events should serialize");
        bytes.push(0);
        assert!(
            read_array::<_, StoredEvent>(
                Cursor::new(&bytes),
                bytes.len() as u64,
                &stored_event_dtype()
            )
            .is_err()
        );

        let wrong_dtype = DType::parse(
            "[('ev', '<u8'), ('exch_ts', '<i8'), ('local_ts', '<i8'), ('px', '<f8'), ('qty', '<i8')]",
        )
        .expect("test dtype should be valid");
        assert!(
            read_array::<_, StoredEvent>(
                Cursor::new(&bytes[..bytes.len() - 1]),
                (bytes.len() - 1) as u64,
                &wrong_dtype
            )
            .is_err()
        );
    }
}
