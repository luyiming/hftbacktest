use std::{
    cell::Cell,
    io::{self, Read, Write},
    rc::Rc,
};

use npyz::{DType, Deserialize, NpyFile, Serialize, WriterBuilder};

struct CountingReader<R> {
    inner: R,
    bytes_read: Rc<Cell<u64>>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.bytes_read.set(
            self.bytes_read
                .get()
                .checked_add(read as u64)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "array size overflow"))?,
        );
        Ok(read)
    }
}

pub fn read_array<R, T>(reader: R, size: u64, expected_dtype: &DType) -> io::Result<Vec<T>>
where
    R: Read,
    T: Deserialize,
{
    let bytes_read = Rc::new(Cell::new(0));
    let counted = CountingReader {
        inner: reader,
        bytes_read: bytes_read.clone(),
    };
    let npy = NpyFile::new(counted)?;
    if npy.shape().len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a one-dimensional numpy array",
        ));
    }
    if &npy.dtype() != expected_dtype {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "numpy dtype mismatch: expected {}, got {}",
                expected_dtype.descr(),
                npy.dtype().descr()
            ),
        ));
    }
    let rows = npy.into_vec::<T>()?;
    if bytes_read.get() != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "numpy payload length mismatch: consumed {}, archive entry has {size}",
                bytes_read.get()
            ),
        ));
    }
    Ok(rows)
}

pub fn write_array<W, T>(writer: W, rows: &[T], dtype: DType) -> io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let len = u64::try_from(rows.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "array length overflow"))?;
    let mut writer = npyz::WriteOptions::new()
        .dtype(dtype)
        .shape(&[len])
        .writer(writer)
        .begin_nd()?;
    for row in rows {
        writer.push(row)?;
    }
    writer.finish()
}
