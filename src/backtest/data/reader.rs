use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsStr,
    io::{self, Error as IoError, ErrorKind},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Weak,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
};

use crate::{backtest::BacktestError, types::Event};

type LoadChunk<D> = fn(&Path) -> io::Result<Vec<D>>;
type Preprocessor<D> = Arc<dyn Fn(&mut [D]) -> io::Result<()> + Send + Sync>;

/// A file or immutable in-memory chunk consumed by a [`Reader`].
#[derive(Clone, Debug)]
pub enum DataSource<D> {
    File(PathBuf),
    Memory(Arc<Vec<D>>),
}

impl<D> From<Vec<D>> for DataSource<D> {
    fn from(data: Vec<D>) -> Self {
        Self::Memory(Arc::new(data))
    }
}

impl<D> From<Arc<Vec<D>>> for DataSource<D> {
    fn from(data: Arc<Vec<D>>) -> Self {
        Self::Memory(data)
    }
}

impl<D> From<PathBuf> for DataSource<D> {
    fn from(path: PathBuf) -> Self {
        Self::File(path)
    }
}

impl<D> From<&Path> for DataSource<D> {
    fn from(path: &Path) -> Self {
        Self::File(path.to_path_buf())
    }
}

enum CacheEntry<D> {
    Loading,
    Ready(Arc<Vec<D>>),
    Shared(Weak<Vec<D>>),
    Failed(Arc<IoError>),
}

#[derive(Clone)]
struct Cache<D>(Rc<RefCell<HashMap<usize, CacheEntry<D>>>>);

impl<D> Default for Cache<D> {
    fn default() -> Self {
        Self(Rc::new(RefCell::new(HashMap::new())))
    }
}

impl<D> Cache<D> {
    fn needs_load(&self, key: usize) -> bool {
        let mut entries = self.0.borrow_mut();
        match entries.get(&key) {
            None => true,
            Some(CacheEntry::Shared(data)) if data.upgrade().is_none() => {
                entries.remove(&key);
                true
            }
            Some(_) => false,
        }
    }

    fn prepare(&self, key: usize) {
        self.0.borrow_mut().insert(key, CacheEntry::Loading);
    }

    fn set(&self, key: usize, data: Arc<Vec<D>>) {
        self.0.borrow_mut().insert(key, CacheEntry::Ready(data));
    }

    fn fail(&self, key: usize, error: IoError) {
        self.0
            .borrow_mut()
            .insert(key, CacheEntry::Failed(Arc::new(error)));
    }

    fn is_loading(&self, key: usize) -> bool {
        matches!(self.0.borrow().get(&key), Some(CacheEntry::Loading))
    }

    fn error(&self, key: usize) -> Option<Arc<IoError>> {
        match self.0.borrow().get(&key) {
            Some(CacheEntry::Failed(error)) => Some(error.clone()),
            _ => None,
        }
    }

    fn checkout(&self, key: usize) -> Option<Arc<Vec<D>>> {
        let mut entries = self.0.borrow_mut();
        let entry = entries.get_mut(&key)?;
        match entry {
            CacheEntry::Ready(data) => {
                let data = data.clone();
                *entry = CacheEntry::Shared(Arc::downgrade(&data));
                Some(data)
            }
            CacheEntry::Shared(data) => data.upgrade(),
            CacheEntry::Loading | CacheEntry::Failed(_) => None,
        }
    }
}

struct LoadResult<D> {
    key: usize,
    result: io::Result<Arc<Vec<D>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("failed to read {path}")]
struct FileLoadError {
    path: PathBuf,
    #[source]
    source: Arc<IoError>,
}

/// Builds a chunk reader around a format-specific loading function.
pub struct ReaderBuilder<D> {
    sources: Vec<DataSource<D>>,
    loader: LoadChunk<D>,
    parallel_load: bool,
    preprocessor: Option<Preprocessor<D>>,
}

impl<D> ReaderBuilder<D>
where
    D: Clone + Send + Sync + 'static,
{
    pub fn new(loader: LoadChunk<D>) -> Self {
        Self {
            sources: Vec::new(),
            loader,
            parallel_load: false,
            preprocessor: None,
        }
    }

    pub fn parallel_load(self, parallel_load: bool) -> Self {
        Self {
            parallel_load,
            ..self
        }
    }

    pub fn preprocess<F>(self, preprocessor: F) -> Self
    where
        F: Fn(&mut [D]) -> io::Result<()> + Send + Sync + 'static,
    {
        Self {
            preprocessor: Some(Arc::new(preprocessor)),
            ..self
        }
    }

    /// Sets chunks in chronological order.
    pub fn data(self, data: Vec<DataSource<D>>) -> Self {
        Self {
            sources: data,
            ..self
        }
    }

    pub fn build(self) -> io::Result<Reader<D>> {
        let sources = if let Some(preprocessor) = &self.preprocessor {
            self.sources
                .into_iter()
                .map(|source| match source {
                    DataSource::File(path) => Ok(DataSource::File(path)),
                    DataSource::Memory(data) => {
                        let mut owned = data.as_ref().clone();
                        preprocessor(&mut owned)?;
                        Ok(DataSource::Memory(Arc::new(owned)))
                    }
                })
                .collect::<io::Result<Vec<_>>>()?
        } else {
            self.sources
        };
        let (sender, receiver) = channel();
        Ok(Reader {
            sources: sources.into(),
            cache: Cache::default(),
            next_source: 0,
            sender,
            receiver: Rc::new(receiver),
            loader: self.loader,
            parallel_load: self.parallel_load,
            preprocessor: self.preprocessor,
        })
    }
}

/// Loads immutable chunks and shares them between independent reader cursors.
#[derive(Clone)]
pub struct Reader<D> {
    sources: Rc<[DataSource<D>]>,
    cache: Cache<D>,
    next_source: usize,
    sender: Sender<LoadResult<D>>,
    receiver: Rc<Receiver<LoadResult<D>>>,
    loader: LoadChunk<D>,
    parallel_load: bool,
    preprocessor: Option<Preprocessor<D>>,
}

impl<D> Reader<D>
where
    D: Clone + Send + Sync + 'static,
{
    pub fn builder(loader: LoadChunk<D>) -> ReaderBuilder<D> {
        ReaderBuilder::new(loader)
    }

    pub fn next_data(&mut self) -> Result<Arc<Vec<D>>, BacktestError> {
        if self.next_source >= self.sources.len() {
            return Err(BacktestError::EndOfData);
        }
        let key = self.next_source;
        loop {
            self.load_data(key)?;
            if self.parallel_load && key + 1 < self.sources.len() {
                self.load_data(key + 1)?;
            }
            while self.cache.is_loading(key) {
                let loaded = self
                    .receiver
                    .recv()
                    .expect("loader sender should remain alive while reader is waiting");
                match loaded.result {
                    Ok(data) => self.cache.set(loaded.key, data),
                    Err(error) => self.cache.fail(loaded.key, error),
                }
            }
            if let Some(error) = self.cache.error(key) {
                let path = match &self.sources[key] {
                    DataSource::File(path) => path.clone(),
                    DataSource::Memory(_) => PathBuf::from("<memory>"),
                };
                return Err(BacktestError::DataError(IoError::new(
                    error.kind(),
                    FileLoadError {
                        path,
                        source: error,
                    },
                )));
            }
            if let Some(data) = self.cache.checkout(key) {
                self.next_source += 1;
                return Ok(data);
            }
        }
    }

    fn load_data(&self, key: usize) -> Result<(), BacktestError> {
        if !self.cache.needs_load(key) {
            return Ok(());
        }
        match &self.sources[key] {
            DataSource::Memory(data) => self.cache.set(key, data.clone()),
            DataSource::File(path) => {
                if path.extension() != Some(OsStr::new("npz")) {
                    return Err(BacktestError::DataError(IoError::new(
                        ErrorKind::InvalidInput,
                        format!("unsupported data file extension: {}", path.display()),
                    )));
                }
                self.cache.prepare(key);
                let sender = self.sender.clone();
                let path = path.clone();
                let loader = self.loader;
                let preprocessor = self.preprocessor.clone();
                let _ = thread::spawn(move || {
                    let result = loader(&path).and_then(|mut data| {
                        if let Some(preprocessor) = preprocessor {
                            preprocessor(&mut data)?;
                        }
                        Ok(Arc::new(data))
                    });
                    let _ = sender.send(LoadResult { key, result });
                });
            }
        }
        Ok(())
    }
}

/// Applies a local timestamp offset before a feed chunk is published.
pub fn adjust_feed_latency(data: &mut [Event], latency_offset: i64) -> io::Result<()> {
    for event in data {
        event.local_ts = event.local_ts.checked_add(latency_offset).ok_or_else(|| {
            IoError::new(ErrorKind::InvalidData, "local timestamp offset overflow")
        })?;
        if event.local_ts <= event.exch_ts {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                "local timestamp must be greater than exchange timestamp after adjustment",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Row(i64);

    static LOADS: AtomicUsize = AtomicUsize::new(0);

    fn load_rows(_path: &Path) -> io::Result<Vec<Row>> {
        LOADS.fetch_add(1, Ordering::Relaxed);
        Ok(vec![Row(1), Row(2)])
    }

    #[test]
    fn cloned_readers_share_published_chunks() {
        LOADS.store(0, Ordering::Relaxed);
        let reader = Reader::builder(load_rows)
            .data(vec![DataSource::File(PathBuf::from("rows.npz"))])
            .build()
            .expect("reader should build");
        let mut first = reader.clone();
        let mut second = reader;
        let first_data = first.next_data().expect("first reader should load");
        let second_data = second.next_data().expect("second reader should share");
        assert!(Arc::ptr_eq(&first_data, &second_data));
        assert_eq!(LOADS.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn preprocesses_memory_before_publishing() {
        let mut reader = Reader::builder(load_rows)
            .data(vec![vec![Row(1), Row(2)].into()])
            .preprocess(|rows| {
                for row in rows {
                    row.0 += 1;
                }
                Ok(())
            })
            .build()
            .expect("reader should build");
        assert_eq!(
            reader
                .next_data()
                .expect("memory data should be available")
                .as_slice(),
            [Row(2), Row(3)]
        );
    }
}
