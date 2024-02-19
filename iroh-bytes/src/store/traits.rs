//! Traits for in-memory or persistent maps of blob with bao encoded outboards.
use std::{collections::BTreeSet, io, path::PathBuf};

use bao_tree::{
    io::fsm::{BaoContentItem, OutboardMut},
    ChunkRanges,
};
use bytes::Bytes;
use futures::{future, Future, Stream};
use genawaiter::rc::{Co, Gen};
use iroh_base::rpc::RpcError;
use iroh_io::{AsyncSliceReader, AsyncSliceWriter};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncRead, sync::mpsc};

use crate::{
    hashseq::parse_hash_seq,
    util::{
        progress::{IdGenerator, ProgressSender},
        Tag,
    },
    BlobFormat, Hash, HashAndFormat, TempTag,
};

pub use bao_tree;
pub use range_collections;

/// A fallible but owned iterator over the entries in a store.
pub type DbIter<T> = Box<dyn Iterator<Item = io::Result<T>> + Send + Sync + 'static>;

/// The availability status of an entry in a store.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum EntryStatus {
    /// The entry is completely available.
    Complete,
    /// The entry is partially available.
    Partial,
    /// The entry is not in the store.
    NotFound,
}

/// An entry in a store that supports partial entries.
///
/// This correspnds to [`EntryStatus`], but also includes the entry itself.
#[derive(Debug)]
pub enum PossiblyPartialEntry<D: PartialMap> {
    /// A complete entry.
    Complete(D::Entry),
    /// A partial entry.
    Partial(D::PartialEntry),
    /// We got nothing.
    NotFound,
}

/// An entry for one hash in a bao collection
///
/// The entry has the ability to provide you with an (outboard, data)
/// reader pair. Creating the reader is async and may fail. The futures that
/// create the readers must be `Send`, but the readers themselves don't have to
/// be.
pub trait MapEntry<D: Map>: Clone + Send + Sync + 'static {
    /// The hash of the entry.
    fn hash(&self) -> Hash;
    /// The size of the entry.
    fn size(&self) -> u64;
    /// Returns `true` if the entry is complete.
    ///
    /// Note that this does not actually verify if the bytes on disk are complete, it only checks
    /// if the entry is among the partial or complete section of the [`Map`]. To verify if all
    /// bytes are actually available on disk, use [`MapEntry::available_ranges`].
    fn is_complete(&self) -> bool;
    /// Compute the available ranges.
    ///
    /// Depending on the implementation, this may be an expensive operation.
    ///
    /// It can also only ever be a best effort, since the underlying data may
    /// change at any time. E.g. somebody could flip a bit in the file, or download
    /// more chunks.
    fn available_ranges(&self) -> impl Future<Output = io::Result<ChunkRanges>> + Send;
    /// A future that resolves to a reader that can be used to read the outboard
    fn outboard(&self) -> impl Future<Output = io::Result<D::Outboard>> + Send;
    /// A future that resolves to a reader that can be used to read the data
    fn data_reader(&self) -> impl Future<Output = io::Result<D::DataReader>> + Send;
}

/// A generic collection of blobs with precomputed outboards
pub trait Map: Clone + Send + Sync + 'static {
    /// The outboard type. This can be an in memory outboard or an outboard that
    /// retrieves the data asynchronously from a remote database.
    type Outboard: bao_tree::io::fsm::Outboard;
    /// The reader type.
    type DataReader: AsyncSliceReader;
    /// The entry type. An entry is a cheaply cloneable handle that can be used
    /// to open readers for both the data and the outboard
    type Entry: MapEntry<Self>;
    /// Get an entry for a hash.
    ///
    /// This can also be used for a membership test by just checking if there
    /// is an entry. Creating an entry should be cheap, any expensive ops should
    /// be deferred to the creation of the actual readers.
    ///
    /// It is not guaranteed that the entry is complete. A [PartialMap] would return
    /// here both complete and partial entries, so that you can share partial entries.
    fn get(&self, hash: &Hash) -> io::Result<Option<Self::Entry>>;
}

/// A partial entry
pub trait PartialMapEntry<D: PartialMap>: MapEntry<D> {
    /// Get a batch writer
    fn batch_writer(&self) -> impl Future<Output = io::Result<D::BatchWriter>> + Send;
}

/// An async batch interface for writing bao content items to a pair of data and
/// outboard.
///
/// Details like the chunk group size and the actual storage location are left
/// to the implementation.
pub trait BaoBatchWriter {
    /// Write a batch of bao content items to the underlying storage.
    ///
    /// The batch is guaranteed to be sorted as data is received from the network.
    /// So leafs will be sorted by offset, and parents will be sorted by pre order
    /// traversal offset. There is no guarantee that they will be consecutive
    /// though.
    ///
    /// The size is the total size of the blob that the remote side told us.
    /// It is not guaranteed to be correct, but it is guaranteed to be
    /// consistent with all data in the batch. The size therefore represents
    /// an upper bound on the maximum offset of all leaf items.
    /// So it is guaranteed that `leaf.offset + leaf.size <= size` for all
    /// leaf items in the batch.
    ///
    /// Batches should not become too large. Typically, a batch is just a few
    /// parent nodes and a leaf.
    ///
    /// Batch is a vec so it can be moved into a task, which is unfortunately
    /// necessary in typical io code.
    fn write_batch(
        &mut self,
        size: u64,
        batch: Vec<BaoContentItem>,
    ) -> impl Future<Output = io::Result<()>>;

    /// Sync the written data to permanent storage, if applicable.
    /// E.g. for a file based implementation, this would call sync_data
    /// on all files.
    fn sync(&mut self) -> impl Future<Output = io::Result<()>>;
}

/// Implement BaoBatchWriter for mutable references
impl<W: BaoBatchWriter> BaoBatchWriter for &mut W {
    async fn write_batch(&mut self, size: u64, batch: Vec<BaoContentItem>) -> io::Result<()> {
        (**self).write_batch(size, batch).await
    }

    async fn sync(&mut self) -> io::Result<()> {
        (**self).sync().await
    }
}

/// A wrapper around a batch writer that calls a progress callback for one leaf
/// per batch.
#[derive(Debug)]
pub struct FallibleProgressBatchWriter<W, F>(W, F);

impl<W: BaoBatchWriter, F: Fn(u64, usize) -> io::Result<()> + 'static>
    FallibleProgressBatchWriter<W, F>
{
    /// Create a new `FallibleProgressBatchWriter` from an inner writer and a progress callback
    ///
    /// The `on_write` function is called for each write, with the `offset` as the first and the
    /// length of the data as the second param. `on_write` must return an `io::Result`.
    /// If `on_write` returns an error, the download is aborted.
    pub fn new(inner: W, on_write: F) -> Self {
        Self(inner, on_write)
    }

    /// Return the inner writer.
    pub fn into_inner(self) -> W {
        self.0
    }
}

impl<W: BaoBatchWriter, F: Fn(u64, usize) -> io::Result<()> + 'static> BaoBatchWriter
    for FallibleProgressBatchWriter<W, F>
{
    async fn write_batch(&mut self, size: u64, batch: Vec<BaoContentItem>) -> io::Result<()> {
        // find the offset and length of the first (usually only) chunk
        let chunk = batch
            .iter()
            .filter_map(|item| {
                if let BaoContentItem::Leaf(leaf) = item {
                    Some((leaf.offset.0, leaf.data.len()))
                } else {
                    None
                }
            })
            .next();
        self.0.write_batch(size, batch).await?;
        // call the progress callback
        if let Some((offset, len)) = chunk {
            (self.1)(offset, len)?;
        }
        Ok(())
    }

    async fn sync(&mut self) -> io::Result<()> {
        self.0.sync().await
    }
}

/// A combined batch writer
///
/// This is just temporary to allow reusing the existing store implementations
/// that have separate data and outboard writers.
#[derive(Debug)]
pub struct CombinedBatchWriter<D, O> {
    /// data part
    pub data: D,
    /// outboard part
    pub outboard: O,
}

impl<D, O> BaoBatchWriter for CombinedBatchWriter<D, O>
where
    D: AsyncSliceWriter,
    O: OutboardMut,
{
    async fn write_batch(&mut self, _size: u64, batch: Vec<BaoContentItem>) -> io::Result<()> {
        for item in batch {
            match item {
                BaoContentItem::Parent(parent) => {
                    self.outboard.save(parent.node, &parent.pair).await?;
                }
                BaoContentItem::Leaf(leaf) => {
                    self.data.write_bytes_at(leaf.offset.0, leaf.data).await?;
                }
            }
        }
        Ok(())
    }

    async fn sync(&mut self) -> io::Result<()> {
        future::try_join(self.data.sync(), self.outboard.sync()).await?;
        Ok(())
    }
}

/// A mutable bao map
pub trait PartialMap: Map {
    /// A partial entry. This is an entry that is writeable and possibly incomplete.
    ///
    /// It must also be readable.
    type PartialEntry: PartialMapEntry<Self>;

    /// The batch writer type
    type BatchWriter: BaoBatchWriter;

    /// Get an existing partial entry, or create a new one.
    ///
    /// We need to know the size of the partial entry. This might produce an
    /// error e.g. if there is not enough space on disk.
    fn get_or_create_partial(&self, hash: Hash, size: u64) -> io::Result<Self::PartialEntry>;

    /// Find out if the data behind a `hash` is complete, partial, or not present.
    ///
    /// Note that this does not actually verify the on-disc data, but only checks in which section
    /// of the store the entry is present.
    fn entry_status(&self, hash: &Hash) -> io::Result<EntryStatus>;

    /// Get an existing entry.
    ///
    /// This will return either a complete entry, a partial entry, or not found.
    ///
    /// This function should not block to perform io. The knowledge about
    /// partial entries must be present in memory.
    fn get_possibly_partial(&self, hash: &Hash) -> io::Result<PossiblyPartialEntry<Self>>;

    /// Upgrade a partial entry to a complete entry.
    fn insert_complete(&self, entry: Self::PartialEntry) -> impl Future<Output = io::Result<()>>;
}

/// Extension of BaoMap to add misc methods used by the rpc calls.
pub trait ReadableStore: Map {
    /// list all blobs in the database. This includes both raw blobs that have
    /// been imported, and hash sequences that have been created internally.
    fn blobs(&self) -> io::Result<DbIter<Hash>>;
    /// list all tags (collections or other explicitly added things) in the database
    fn tags(&self) -> io::Result<DbIter<(Tag, HashAndFormat)>>;

    /// Temp tags
    fn temp_tags(&self) -> Box<dyn Iterator<Item = HashAndFormat> + Send + Sync + 'static>;

    /// Validate the database
    fn validate(
        &self,
        tx: mpsc::Sender<ValidateProgress>,
    ) -> impl Future<Output = io::Result<()>> + Send;

    /// list partial blobs in the database
    fn partial_blobs(&self) -> io::Result<DbIter<Hash>>;

    /// This trait method extracts a file to a local path.
    ///
    /// `hash` is the hash of the file
    /// `target` is the path to the target file
    /// `mode` is a hint how the file should be exported.
    /// `progress` is a callback that is called with the total number of bytes that have been written
    fn export(
        &self,
        hash: Hash,
        target: PathBuf,
        mode: ExportMode,
        progress: impl Fn(u64) -> io::Result<()> + Send + Sync + 'static,
    ) -> impl Future<Output = io::Result<()>> + Send;
}

/// The mutable part of a BaoDb
pub trait Store: ReadableStore + PartialMap {
    /// This trait method imports a file from a local path.
    ///
    /// `data` is the path to the file.
    /// `mode` is a hint how the file should be imported.
    /// `progress` is a sender that provides a way for the importer to send progress messages
    /// when importing large files. This also serves as a way to cancel the import. If the
    /// consumer of the progress messages is dropped, subsequent attempts to send progress
    /// will fail.
    ///
    /// Returns the hash of the imported file. The reason to have this method is that some database
    /// implementations might be able to import a file without copying it.
    fn import_file(
        &self,
        data: PathBuf,
        mode: ImportMode,
        format: BlobFormat,
        progress: impl ProgressSender<Msg = ImportProgress> + IdGenerator,
    ) -> impl Future<Output = io::Result<(TempTag, u64)>> + Send;

    /// Import data from memory.
    ///
    /// It is a special case of `import` that does not use the file system.
    fn import_bytes(
        &self,
        bytes: Bytes,
        format: BlobFormat,
    ) -> impl Future<Output = io::Result<TempTag>> + Send;

    /// Import data from a stream of bytes.
    fn import_stream(
        &self,
        data: impl Stream<Item = io::Result<Bytes>> + Send + Unpin + 'static,
        format: BlobFormat,
        progress: impl ProgressSender<Msg = ImportProgress> + IdGenerator,
    ) -> impl Future<Output = io::Result<(TempTag, u64)>> + Send;

    /// Import data from an async byte reader.
    fn import_reader(
        &self,
        data: impl AsyncRead + Send + Unpin + 'static,
        format: BlobFormat,
        progress: impl ProgressSender<Msg = ImportProgress> + IdGenerator,
    ) -> impl Future<Output = io::Result<(TempTag, u64)>> + Send {
        let stream = tokio_util::io::ReaderStream::new(data);
        self.import_stream(stream, format, progress)
    }

    /// Set a tag
    fn set_tag(
        &self,
        name: Tag,
        hash: Option<HashAndFormat>,
    ) -> impl Future<Output = io::Result<()>> + Send;

    /// Create a new tag
    fn create_tag(&self, hash: HashAndFormat) -> impl Future<Output = io::Result<Tag>> + Send;

    /// Create a temporary pin for this store
    fn temp_tag(&self, value: HashAndFormat) -> TempTag;

    /// Traverse all roots recursively and mark them as live.
    ///
    /// Poll this stream to completion to perform a full gc mark phase.
    ///
    /// Not polling this stream to completion is dangerous, since it might lead
    /// to some live data being missed.
    ///
    /// The implementation of this method should do the minimum amount of work
    /// to determine the live set. Actual deletion of garbage should be done
    /// in the gc_sweep phase.
    fn gc_mark(
        &self,
        extra_roots: impl IntoIterator<Item = io::Result<HashAndFormat>>,
    ) -> impl Stream<Item = GcMarkEvent> + Unpin {
        Gen::new(|co| async move {
            if let Err(e) = gc_mark_task(self, extra_roots, &co).await {
                co.yield_(GcMarkEvent::Error(e)).await;
            }
        })
    }

    /// Remove all blobs that are not marked as live.
    ///
    /// Poll this stream to completion to perform a full gc sweep. Not polling this stream
    /// to completion just means that some garbage will remain in the database.
    ///
    /// Sweeping might take long, but it can safely be done in the background.
    fn gc_sweep(&self) -> impl Stream<Item = GcSweepEvent> + Unpin {
        Gen::new(|co| async move {
            if let Err(e) = gc_sweep_task(self, &co).await {
                co.yield_(GcSweepEvent::Error(e)).await;
            }
        })
    }

    /// Clear the live set.
    fn clear_live(&self);

    /// Add the given hashes to the live set.
    ///
    /// This is used by the gc mark phase to mark roots as live.
    fn add_live(&self, live: impl IntoIterator<Item = Hash>);

    /// True if the given hash is live.
    fn is_live(&self, hash: &Hash) -> bool;

    /// physically delete the given hashes from the store.
    fn delete(&self, hashes: Vec<Hash>) -> impl Future<Output = io::Result<()>> + Send;
}

/// Implementation of the gc method.
async fn gc_mark_task<'a>(
    store: &'a impl Store,
    extra_roots: impl IntoIterator<Item = io::Result<HashAndFormat>> + 'a,
    co: &Co<GcMarkEvent>,
) -> anyhow::Result<()> {
    macro_rules! debug {
        ($($arg:tt)*) => {
            co.yield_(GcMarkEvent::CustomDebug(format!($($arg)*))).await;
        };
    }
    macro_rules! warn {
        ($($arg:tt)*) => {
            co.yield_(GcMarkEvent::CustomWarning(format!($($arg)*), None)).await;
        };
    }
    let mut roots = BTreeSet::new();
    debug!("traversing tags");
    for item in store.tags()? {
        let (name, haf) = item?;
        debug!("adding root {:?} {:?}", name, haf);
        roots.insert(haf);
    }
    debug!("traversing temp roots");
    for haf in store.temp_tags() {
        debug!("adding temp pin {:?}", haf);
        roots.insert(haf);
    }
    debug!("traversing extra roots");
    for haf in extra_roots {
        let haf = haf?;
        debug!("adding extra root {:?}", haf);
        roots.insert(haf);
    }
    let mut live: BTreeSet<Hash> = BTreeSet::new();
    for HashAndFormat { hash, format } in roots {
        // we need to do this for all formats except raw
        if live.insert(hash) && !format.is_raw() {
            let Some(entry) = store.get(&hash)? else {
                warn!("gc: {} not found", hash);
                continue;
            };
            if !entry.is_complete() {
                warn!("gc: {} is partial", hash);
                continue;
            }
            let Ok(reader) = entry.data_reader().await else {
                warn!("gc: {} creating data reader failed", hash);
                continue;
            };
            let Ok((mut stream, count)) = parse_hash_seq(reader).await else {
                warn!("gc: {} parse failed", hash);
                continue;
            };
            debug!("parsed collection {} {:?}", hash, count);
            loop {
                let item = match stream.next().await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(_err) => {
                        warn!("gc: {} parse failed", hash);
                        break;
                    }
                };
                // if format != raw we would have to recurse here by adding this to current
                live.insert(item);
            }
        }
    }
    debug!("gc mark done. found {} live blobs", live.len());
    store.add_live(live);
    Ok(())
}

async fn gc_sweep_task<'a>(store: &'a impl Store, co: &Co<GcSweepEvent>) -> anyhow::Result<()> {
    let blobs = store.blobs()?.chain(store.partial_blobs()?);
    let mut count = 0;
    let mut batch = Vec::new();
    for hash in blobs {
        let hash = hash?;
        if !store.is_live(&hash) {
            batch.push(hash);
            count += 1;
        }
        if batch.len() >= 100 {
            store.delete(batch.clone()).await?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        store.delete(batch).await?;
    }
    co.yield_(GcSweepEvent::CustomDebug(format!(
        "deleted {} blobs",
        count
    )))
    .await;
    Ok(())
}

/// An event related to GC
#[derive(Debug)]
pub enum GcMarkEvent {
    /// A custom event (info)
    CustomDebug(String),
    /// A custom non critical error
    CustomWarning(String, Option<anyhow::Error>),
    /// An unrecoverable error during GC
    Error(anyhow::Error),
}

/// An event related to GC
#[derive(Debug)]
pub enum GcSweepEvent {
    /// A custom event (debug)
    CustomDebug(String),
    /// A custom non critical error
    CustomWarning(String, Option<anyhow::Error>),
    /// An unrecoverable error during GC
    Error(anyhow::Error),
}

/// Progress messages for an import operation
///
/// An import operation involves computing the outboard of a file, and then
/// either copying or moving the file into the database.
#[allow(missing_docs)]
#[derive(Debug)]
pub enum ImportProgress {
    /// Found a path
    ///
    /// This will be the first message for an id
    Found { id: u64, name: String },
    /// Progress when copying the file to the store
    ///
    /// This will be omitted if the store can use the file in place
    ///
    /// There will be multiple of these messages for an id
    CopyProgress { id: u64, offset: u64 },
    /// Determined the size
    ///
    /// This will come after `Found` and zero or more `CopyProgress` messages.
    /// For unstable files, determining the size will only be done once the file
    /// is fully copied.
    Size { id: u64, size: u64 },
    /// Progress when computing the outboard
    ///
    /// There will be multiple of these messages for an id
    OutboardProgress { id: u64, offset: u64 },
    /// Done computing the outboard
    ///
    /// This comes after `Size` and zero or more `OutboardProgress` messages
    OutboardDone { id: u64, hash: Hash },
}

/// The import mode describes how files will be imported.
///
/// This is a hint to the import trait method. For some implementations, this
/// does not make any sense. E.g. an in memory implementation will always have
/// to copy the file into memory. Also, a disk based implementation might choose
/// to copy small files even if the mode is `Reference`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportMode {
    /// This mode will copy the file into the database before hashing.
    ///
    /// This is the safe default because the file can not be accidentally modified
    /// after it has been imported.
    #[default]
    Copy,
    /// This mode will try to reference the file in place and assume it is unchanged after import.
    ///
    /// This has a large performance and storage benefit, but it is less safe since
    /// the file might be modified after it has been imported.
    ///
    /// Stores are allowed to ignore this mode and always copy the file, e.g.
    /// if the file is very small or if the store does not support referencing files.
    TryReference,
}
/// The import mode describes how files will be imported.
///
/// This is a hint to the import trait method. For some implementations, this
/// does not make any sense. E.g. an in memory implementation will always have
/// to copy the file into memory. Also, a disk based implementation might choose
/// to copy small files even if the mode is `Reference`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
pub enum ExportMode {
    /// This mode will copy the file to the target directory.
    ///
    /// This is the safe default because the file can not be accidentally modified
    /// after it has been exported.
    #[default]
    Copy,
    /// This mode will try to move the file to the target directory and then reference it from
    /// the database.
    ///
    /// This has a large performance and storage benefit, but it is less safe since
    /// the file might be modified in the target directory after it has been exported.
    ///
    /// Stores are allowed to ignore this mode and always copy the file, e.g.
    /// if the file is very small or if the store does not support referencing files.
    TryReference,
}

#[allow(missing_docs)]
#[derive(Debug)]
pub enum ExportProgress {
    /// Starting to export to a file
    ///
    /// This will be the first message for an id
    Start {
        id: u64,
        hash: Hash,
        path: PathBuf,
        stable: bool,
    },
    /// Progress when copying the file to the target
    ///
    /// This will be omitted if the store can move the file or use copy on write
    ///
    /// There will be multiple of these messages for an id
    Progress { id: u64, offset: u64 },
    /// Done exporting
    Done { id: u64 },
}

/// Progress updates for the provide operation
#[derive(Debug, Serialize, Deserialize)]
pub enum ValidateProgress {
    /// started validating
    Starting {
        /// The total number of entries to validate
        total: u64,
    },
    /// We started validating an entry
    Entry {
        /// a new unique id for this entry
        id: u64,
        /// the hash of the entry
        hash: Hash,
        /// location of the entry.
        ///
        /// In case of a file, this is the path to the file.
        /// Otherwise it might be an url or something else to uniquely identify the entry.
        path: Option<String>,
        /// the size of the entry
        size: u64,
    },
    /// We got progress ingesting item `id`.
    Progress {
        /// The unique id of the entry.
        id: u64,
        /// The offset of the progress, in bytes.
        offset: u64,
    },
    /// We are done with `id`
    Done {
        /// The unique id of the entry.
        id: u64,
        /// An error if we failed to validate the entry.
        error: Option<String>,
    },
    /// We are done with the whole operation.
    AllDone,
    /// We got an error and need to abort.
    Abort(RpcError),
}

/// Database events
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A GC was started
    GcStarted,
    /// A GC was completed
    GcCompleted,
}
