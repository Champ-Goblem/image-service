// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Struct to maintain context information for the image builder.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::fmt;
use std::fs::{remove_file, rename, File, OpenOptions};
use std::io::{BufWriter, Cursor, Read, Seek, Write};
use std::path::PathBuf;
use std::path::{Display, Path};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Error, Result};
use sha2::{Digest, Sha256};
use tar::{EntryType, Header};
use vmm_sys_util::tempfile::TempFile;

use nydus_rafs::metadata::chunk::ChunkWrapper;
use nydus_rafs::metadata::layout::v5::RafsV5BlobTable;
use nydus_rafs::metadata::layout::v6::{RafsV6BlobTable, EROFS_BLOCK_SIZE, EROFS_INODE_SLOT_SIZE};
use nydus_rafs::metadata::layout::RafsBlobTable;
use nydus_rafs::metadata::{Inode, RAFS_DEFAULT_CHUNK_SIZE};
use nydus_rafs::metadata::{RafsSuperFlags, RafsVersion};
use nydus_rafs::{RafsIoReader, RafsIoWrite};
use nydus_storage::device::{BlobFeatures, BlobInfo};
use nydus_storage::meta::{
    BlobChunkInfoV2Ondisk, BlobMetaChunkArray, BlobMetaHeaderOndisk, ZranContextGenerator,
    BLOB_META_FEATURE_4K_ALIGNED, BLOB_META_FEATURE_CHUNK_INFO_V2, BLOB_META_FEATURE_SEPARATE,
    BLOB_META_FEATURE_ZRAN,
};
use nydus_utils::{compress, digest, div_round_up, round_down_4k};

use super::chunk_dict::{ChunkDict, HashChunkDict};
use super::node::{ChunkSource, Node, WhiteoutSpec};
use super::prefetch::{Prefetch, PrefetchPolicy};

// TODO: select BufWriter capacity by performance testing.
pub const BUF_WRITER_CAPACITY: usize = 2 << 17;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversionType {
    DirectoryToRafs,
    DirectoryToStargz,
    DirectoryToTargz,
    EStargzToRafs,
    EStargzToRef,
    EStargzIndexToRef,
    TargzToRafs,
    TargzToStargz,
    TargzToRef,
    TarToStargz,
    TarToRafs,
}

impl Default for ConversionType {
    fn default() -> Self {
        Self::DirectoryToRafs
    }
}

impl FromStr for ConversionType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "dir-rafs" => Ok(Self::DirectoryToRafs),
            "dir-stargz" => Ok(Self::DirectoryToStargz),
            "dir-targz" => Ok(Self::DirectoryToTargz),
            "estargz-rafs" => Ok(Self::EStargzToRafs),
            "estargz-ref" => Ok(Self::EStargzToRef),
            "estargztoc-ref" => Ok(Self::EStargzIndexToRef),
            "targz-rafs" => Ok(Self::TargzToRafs),
            "targz-stargz" => Ok(Self::TargzToStargz),
            "targz-ref" => Ok(Self::TargzToRef),
            "tar-rafs" => Ok(Self::TarToRafs),
            "tar-stargz" => Ok(Self::TarToStargz),
            // kept for backward compatibility
            "directory" => Ok(Self::DirectoryToRafs),
            "stargz_index" => Ok(Self::EStargzIndexToRef),
            _ => Err(anyhow!("invalid conversion type")),
        }
    }
}

impl fmt::Display for ConversionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConversionType::DirectoryToRafs => write!(f, "dir-rafs"),
            ConversionType::DirectoryToStargz => write!(f, "dir-stargz"),
            ConversionType::DirectoryToTargz => write!(f, "dir-targz"),
            ConversionType::EStargzToRafs => write!(f, "estargz-rafs"),
            ConversionType::EStargzToRef => write!(f, "estargz-ref"),
            ConversionType::EStargzIndexToRef => write!(f, "estargztoc-ref"),
            ConversionType::TargzToRafs => write!(f, "targz-rafs"),
            ConversionType::TargzToStargz => write!(f, "targz-ref"),
            ConversionType::TargzToRef => write!(f, "targz-ref"),
            ConversionType::TarToRafs => write!(f, "tar-rafs"),
            ConversionType::TarToStargz => write!(f, "tar-stargz"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ArtifactStorage {
    // Won't rename user's specification
    SingleFile(PathBuf),
    // Will rename it from tmp file as user didn't specify a name.
    FileDir(PathBuf),
}

impl ArtifactStorage {
    pub fn display(&self) -> Display {
        match self {
            ArtifactStorage::SingleFile(p) => p.display(),
            ArtifactStorage::FileDir(p) => p.display(),
        }
    }
}

impl Default for ArtifactStorage {
    fn default() -> Self {
        Self::SingleFile(PathBuf::new())
    }
}

/// ArtifactMemoryWriter provides a writer to allow writing bootstrap
/// data to a byte slice in memory.
pub struct ArtifactMemoryWriter(Cursor<Vec<u8>>);

impl RafsIoWrite for ArtifactMemoryWriter {
    fn as_any(&self) -> &dyn Any {
        &self.0
    }

    fn as_reader(&mut self) -> std::io::Result<&mut dyn Read> {
        self.0.set_position(0);
        Ok(&mut self.0)
    }
}

impl Seek for ArtifactMemoryWriter {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl Write for ArtifactMemoryWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

pub struct ArtifactFileWriter(ArtifactWriter);

impl RafsIoWrite for ArtifactFileWriter {
    fn as_any(&self) -> &dyn Any {
        &self.0
    }

    fn finalize(&mut self, name: Option<String>) -> Result<()> {
        self.0.finalize(name)
    }

    fn as_reader(&mut self) -> std::io::Result<&mut dyn Read> {
        self.0.file.flush()?;
        self.0.reader.seek_offset(0)?;
        Ok(&mut self.0.reader)
    }
}

impl Seek for ArtifactFileWriter {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.file.seek(pos)
    }
}

impl Write for ArtifactFileWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// ArtifactWriter provides a writer to allow writing bootstrap
/// or blob data to a single file or in a directory.
pub struct ArtifactWriter {
    pos: usize,
    file: BufWriter<File>,
    reader: File,
    storage: ArtifactStorage,
    // Keep this because tmp file will be removed automatically when it is dropped.
    // But we will rename/link the tmp file before it is removed.
    tmp_file: Option<TempFile>,
}

impl Write for ArtifactWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(bytes)?;
        self.pos += n;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl ArtifactWriter {
    pub fn new(storage: ArtifactStorage, fifo: bool) -> Result<Self> {
        match storage {
            ArtifactStorage::SingleFile(ref p) => {
                let mut opener = &mut OpenOptions::new();
                opener = opener.write(true).create(true);
                // Make it as the writer side of FIFO file, no truncate flag because it has
                // been created by the reader side.
                if !fifo {
                    opener = opener.truncate(true);
                }
                let b = BufWriter::with_capacity(
                    BUF_WRITER_CAPACITY,
                    opener
                        .open(p)
                        .with_context(|| format!("failed to open file {}", p.display()))?,
                );
                let reader = OpenOptions::new()
                    .read(true)
                    .open(p)
                    .with_context(|| format!("failed to open file {}", p.display()))?;
                Ok(Self {
                    pos: 0,
                    file: b,
                    reader,
                    storage,
                    tmp_file: None,
                })
            }
            ArtifactStorage::FileDir(ref p) => {
                // Better we can use open(2) O_TMPFILE, but for compatibility sake, we delay this job.
                // TODO: Blob dir existence?
                let tmp = TempFile::new_in(p)
                    .with_context(|| format!("failed to create temp file in {}", p.display()))?;
                let tmp2 = tmp.as_file().try_clone()?;
                let reader = OpenOptions::new()
                    .read(true)
                    .open(tmp.as_path())
                    .with_context(|| format!("failed to open file {}", tmp.as_path().display()))?;
                Ok(Self {
                    pos: 0,
                    file: BufWriter::with_capacity(BUF_WRITER_CAPACITY, tmp2),
                    reader,
                    storage,
                    tmp_file: Some(tmp),
                })
            }
        }
    }

    pub fn pos(&self) -> Result<u64> {
        Ok(self.pos as u64)
    }

    // The `inline-bootstrap` option merges the blob and bootstrap into one
    // file. We need some header to index the location of the blob and bootstrap,
    // write_tar_header uses tar header that arranges the data as follows:
    //      blob_data | blob_tar_header | bootstrap_data | bootstrap_tar_header
    // This is a tar-like structure, except that we put the tar header after the
    // data. The advantage is that we do not need to determine the size of the data
    // first, so that we can write the blob data by stream without seek to improve
    // the performance of the blob dump by using fifo, if we need to read the bootstrap
    // data quickly, first need to read the 512 bytes tar header from the end of blob
    // file first, and then seek offset to read bootstrap data.
    pub fn write_tar_header(&mut self, name: &str, size: u64) -> Result<Header> {
        let mut header = Header::new_gnu();
        header.set_path(Path::new(name))?;
        header.set_entry_type(EntryType::Regular);
        header.set_size(size);
        // The checksum must be set to ensure that the tar reader implementation
        // in golang can correctly parse the header.
        header.set_cksum();
        self.write_all(header.as_bytes())?;
        Ok(header)
    }

    /// Finalize the metadata/data blob.
    ///
    /// When `name` is None, it means that the blob is empty and should be removed.
    pub fn finalize(&mut self, name: Option<String>) -> Result<()> {
        self.file.flush()?;

        if let Some(n) = name {
            if let ArtifactStorage::FileDir(s) = &self.storage {
                let path = Path::new(s).join(n);
                if !path.exists() {
                    if let Some(tmp_file) = &self.tmp_file {
                        rename(tmp_file.as_path(), &path).with_context(|| {
                            format!(
                                "failed to rename blob {:?} to {:?}",
                                tmp_file.as_path(),
                                path
                            )
                        })?;
                    }
                }
            }
        } else if let ArtifactStorage::SingleFile(s) = &self.storage {
            if let Ok(md) = s.metadata() {
                if md.is_file() {
                    remove_file(s).with_context(|| format!("failed to remove blob {:?}", s))?;
                }
            }
        }

        Ok(())
    }
}

/// BlobContext is used to hold the blob information of a layer during build.
pub struct BlobContext {
    /// Blob id (user specified or sha256(blob)).
    pub blob_id: String,
    pub blob_hash: Sha256,
    pub blob_prefetch_size: u64,
    /// Whether to generate blob metadata information.
    pub blob_meta_info_enabled: bool,
    /// Data chunks stored in the data blob, for v6.
    pub blob_meta_info: BlobMetaChunkArray,
    /// Blob metadata header stored in the data blob, for v6
    pub blob_meta_header: BlobMetaHeaderOndisk,

    /// Final compressed blob file size.
    pub compressed_blob_size: u64,
    /// Final expected blob cache file size.
    pub uncompressed_blob_size: u64,

    /// Current blob offset cursor for writing to disk file.
    pub compressed_offset: u64,
    pub uncompressed_offset: u64,

    /// The number of counts in a blob by the index of blob table.
    pub chunk_count: u32,
    /// Chunk slice size.
    pub chunk_size: u32,
    /// Whether the blob is from chunk dict.
    pub chunk_source: ChunkSource,
}

impl BlobContext {
    pub fn new(blob_id: String, blob_offset: u64, features: u32) -> Self {
        let blob_meta_info = if features & BLOB_META_FEATURE_CHUNK_INFO_V2 != 0 {
            BlobMetaChunkArray::new_v2()
        } else {
            BlobMetaChunkArray::new_v1()
        };
        let mut blob_ctx = Self {
            blob_id,
            blob_hash: Sha256::new(),
            blob_prefetch_size: 0,
            blob_meta_info_enabled: false,
            blob_meta_info,
            blob_meta_header: BlobMetaHeaderOndisk::default(),

            compressed_blob_size: 0,
            uncompressed_blob_size: 0,

            compressed_offset: blob_offset,
            uncompressed_offset: 0,

            chunk_count: 0,
            chunk_size: RAFS_DEFAULT_CHUNK_SIZE as u32,
            chunk_source: ChunkSource::Build,
        };

        if features & BLOB_META_FEATURE_4K_ALIGNED != 0 {
            blob_ctx.blob_meta_header.set_4k_aligned(true);
        }
        if features & BLOB_META_FEATURE_SEPARATE != 0 {
            blob_ctx.blob_meta_header.set_ci_separate(true);
        }
        if features & BLOB_META_FEATURE_CHUNK_INFO_V2 != 0 {
            blob_ctx.blob_meta_header.set_chunk_info_v2(true);
        }
        if features & BLOB_META_FEATURE_ZRAN != 0 {
            blob_ctx.blob_meta_header.set_ci_zran(true);
        }

        blob_ctx
    }

    pub fn from(ctx: &BuildContext, blob: &BlobInfo, chunk_source: ChunkSource) -> Self {
        let mut blob_ctx = Self::new(blob.blob_id().to_owned(), 0, blob.meta_flags());

        blob_ctx.blob_prefetch_size = blob.prefetch_size();
        blob_ctx.chunk_count = blob.chunk_count();
        blob_ctx.uncompressed_blob_size = blob.uncompressed_size();
        blob_ctx.compressed_blob_size = blob.compressed_size();
        blob_ctx.chunk_size = blob.chunk_size();
        blob_ctx.chunk_source = chunk_source;
        blob_ctx.blob_meta_header.set_4k_aligned(ctx.aligned_chunk);

        if blob.meta_ci_is_valid() {
            blob_ctx
                .blob_meta_header
                .set_ci_compressor(blob.meta_ci_compressor());
            blob_ctx.blob_meta_header.set_ci_entries(blob.chunk_count());
            blob_ctx
                .blob_meta_header
                .set_ci_compressed_offset(blob.meta_ci_offset());
            blob_ctx
                .blob_meta_header
                .set_ci_compressed_size(blob.meta_ci_compressed_size());
            blob_ctx
                .blob_meta_header
                .set_ci_uncompressed_size(blob.meta_ci_uncompressed_size());
            blob_ctx.blob_meta_header.set_4k_aligned(true);
            blob_ctx
                .blob_meta_header
                .set_chunk_info_v2(blob.meta_flags() & BLOB_META_FEATURE_CHUNK_INFO_V2 != 0);
            blob_ctx.blob_meta_info_enabled = true;
        }

        blob_ctx
    }

    pub fn set_chunk_size(&mut self, chunk_size: u32) {
        self.chunk_size = chunk_size;
    }

    // TODO: check the logic to reset prefetch size
    pub fn set_blob_prefetch_size(&mut self, ctx: &BuildContext) {
        if (self.compressed_blob_size > 0
            || (ctx.conversion_type == ConversionType::EStargzIndexToRef
                && !self.blob_id.is_empty()))
            && ctx.prefetch.policy != PrefetchPolicy::Blob
        {
            self.blob_prefetch_size = 0;
        }
    }

    pub fn set_meta_info_enabled(&mut self, enable: bool) {
        self.blob_meta_info_enabled = enable;
    }

    pub fn add_chunk_meta_info(
        &mut self,
        chunk: &ChunkWrapper,
        chunk_info: Option<BlobChunkInfoV2Ondisk>,
    ) -> Result<()> {
        if self.blob_meta_info_enabled {
            assert_eq!(chunk.index() as usize, self.blob_meta_info.len());
            match &self.blob_meta_info {
                BlobMetaChunkArray::V1(_) => {
                    self.blob_meta_info.add_v1(
                        chunk.compressed_offset(),
                        chunk.compressed_size(),
                        chunk.uncompressed_offset(),
                        chunk.uncompressed_size(),
                    );
                }
                BlobMetaChunkArray::V2(_) => {
                    if let Some(info) = chunk_info {
                        self.blob_meta_info.add_v2_info(info);
                    } else {
                        self.blob_meta_info.add_v2(
                            chunk.compressed_offset(),
                            chunk.compressed_size(),
                            chunk.uncompressed_offset(),
                            chunk.uncompressed_size(),
                            chunk.is_compressed(),
                            0,
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Allocate a count index sequentially in a blob.
    pub fn alloc_chunk_index(&mut self) -> Result<u32> {
        let index = self.chunk_count;

        // Rafs v6 only supports 24 bit chunk id.
        if index >= 0xff_ffff {
            Err(Error::msg(
                "the number of chunks in blob exceeds the u32 limit",
            ))
        } else {
            self.chunk_count += 1;
            Ok(index)
        }
    }

    pub fn blob_id(&mut self) -> Option<String> {
        if self.compressed_blob_size > 0 {
            Some(self.blob_id.to_string())
        } else {
            None
        }
    }
}

/// BlobManager stores all blob related information during build.
pub struct BlobManager {
    /// Some layers may not have a blob (only have metadata), so Option
    /// is used here, the vector index will be as the layer index.
    ///
    /// We can get blob index for a layer by using:
    /// `self.blobs.iter().flatten().collect()[layer_index];`
    blobs: Vec<BlobContext>,
    current_blob_index: Option<u32>,
    /// Chunk dictionary to hold chunks from an extra chunk dict file.
    /// Used for chunk data de-duplication within the whole image.
    pub global_chunk_dict: Arc<dyn ChunkDict>,
    /// Chunk dictionary to hold chunks from all layers.
    /// Used for chunk data de-duplication between layers (with `--parent-bootstrap`)
    /// or within layer (with `--inline-bootstrap`).
    pub layered_chunk_dict: HashChunkDict,
}

impl BlobManager {
    pub fn new() -> Self {
        Self {
            blobs: Vec::new(),
            current_blob_index: None,
            global_chunk_dict: Arc::new(()),
            layered_chunk_dict: HashChunkDict::default(),
        }
    }

    fn new_blob_ctx(ctx: &BuildContext) -> Result<BlobContext> {
        let mut blob_ctx =
            BlobContext::new(ctx.blob_id.clone(), ctx.blob_offset, ctx.blob_meta_features);
        blob_ctx.set_chunk_size(ctx.chunk_size);
        blob_ctx.set_meta_info_enabled(ctx.fs_version == RafsVersion::V6);

        Ok(blob_ctx)
    }

    pub fn get_or_create_current_blob(
        &mut self,
        ctx: &BuildContext,
    ) -> Result<(u32, &mut BlobContext)> {
        if self.current_blob_index.is_none() {
            let blob_ctx = Self::new_blob_ctx(ctx)?;
            self.current_blob_index = Some(self.alloc_index()?);
            self.add(blob_ctx);
        }
        // Safe to unwrap because the blob context has been added.
        Ok(self.get_current_blob().unwrap())
    }

    pub fn get_current_blob(&mut self) -> Option<(u32, &mut BlobContext)> {
        if let Some(idx) = self.current_blob_index {
            Some((idx, &mut self.blobs[idx as usize]))
        } else {
            None
        }
    }

    pub fn set_chunk_dict(&mut self, dict: Arc<dyn ChunkDict>) {
        self.global_chunk_dict = dict
    }

    pub fn get_chunk_dict(&self) -> Arc<dyn ChunkDict> {
        self.global_chunk_dict.clone()
    }

    /// Allocate a blob index sequentially.
    ///
    /// This should be paired with Self::add() and keep in consistence.
    pub fn alloc_index(&self) -> Result<u32> {
        // Rafs v6 only supports 256 blobs.
        u8::try_from(self.blobs.len())
            .map(|v| v as u32)
            .with_context(|| Error::msg("too many blobs"))
    }

    /// Add a blob context to manager
    ///
    /// This should be paired with Self::alloc_index() and keep in consistence.
    pub fn add(&mut self, blob_ctx: BlobContext) {
        self.blobs.push(blob_ctx);
    }

    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    /// Get all blob contexts (include the blob context that does not have a blob).
    pub fn get_blobs(&self) -> Vec<&BlobContext> {
        self.blobs.iter().collect()
    }

    pub fn get_blob(&self, idx: usize) -> Option<&BlobContext> {
        self.blobs.get(idx)
    }

    pub fn take_blob(&mut self, idx: usize) -> BlobContext {
        self.blobs.remove(idx)
    }

    pub fn get_last_blob(&self) -> Option<&BlobContext> {
        self.blobs.last()
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn from_blob_table(&mut self, ctx: &BuildContext, blob_table: Vec<Arc<BlobInfo>>) {
        self.blobs = blob_table
            .iter()
            .map(|entry| BlobContext::from(ctx, entry.as_ref(), ChunkSource::Parent))
            .collect();
    }

    pub fn get_blob_idx_by_id(&self, id: &str) -> Option<u32> {
        for (idx, blob) in self.blobs.iter().enumerate() {
            if blob.blob_id.eq(id) {
                return Some(idx as u32);
            }
        }
        None
    }

    pub fn get_blob_ids(&self) -> Vec<String> {
        self.blobs.iter().map(|b| b.blob_id.to_owned()).collect()
    }

    /// Extend blobs which belong to ChunkDict and setup real_blob_idx map
    /// should call this function after import parent bootstrap
    /// otherwise will break blobs order
    pub fn extend_blob_table_from_chunk_dict(&mut self, ctx: &BuildContext) -> Result<()> {
        let blobs = self.global_chunk_dict.get_blobs();

        for blob in blobs.iter() {
            if let Some(real_idx) = self.get_blob_idx_by_id(blob.blob_id()) {
                self.global_chunk_dict
                    .set_real_blob_idx(blob.blob_index(), real_idx);
            } else {
                let idx = self.alloc_index()?;
                self.add(BlobContext::from(ctx, blob.as_ref(), ChunkSource::Dict));
                self.global_chunk_dict
                    .set_real_blob_idx(blob.blob_index(), idx);
            }
        }

        Ok(())
    }

    pub fn to_blob_table(&self, build_ctx: &BuildContext) -> Result<RafsBlobTable> {
        let mut blob_table = match build_ctx.fs_version {
            RafsVersion::V5 => RafsBlobTable::V5(RafsV5BlobTable::new()),
            RafsVersion::V6 => RafsBlobTable::V6(RafsV6BlobTable::new()),
        };

        for ctx in &self.blobs {
            let blob_id = ctx.blob_id.clone();
            let blob_prefetch_size = u32::try_from(ctx.blob_prefetch_size)?;
            let chunk_count = ctx.chunk_count;
            let decompressed_blob_size = ctx.uncompressed_blob_size;
            let compressed_blob_size = ctx.compressed_blob_size;
            let blob_features = BlobFeatures::empty();
            let mut flags = RafsSuperFlags::empty();
            match &mut blob_table {
                RafsBlobTable::V5(table) => {
                    flags |= RafsSuperFlags::from(build_ctx.compressor);
                    flags |= RafsSuperFlags::from(build_ctx.digester);
                    table.add(
                        blob_id,
                        0,
                        blob_prefetch_size,
                        ctx.chunk_size,
                        chunk_count,
                        decompressed_blob_size,
                        compressed_blob_size,
                        blob_features,
                        flags,
                    );
                }
                RafsBlobTable::V6(table) => {
                    flags |= RafsSuperFlags::from(build_ctx.compressor);
                    flags |= RafsSuperFlags::from(build_ctx.digester);
                    table.add(
                        blob_id,
                        0,
                        blob_prefetch_size,
                        ctx.chunk_size,
                        chunk_count,
                        decompressed_blob_size,
                        compressed_blob_size,
                        blob_features,
                        flags,
                        ctx.blob_meta_header,
                    );
                }
            }
        }

        Ok(blob_table)
    }
}

/// BootstrapContext is used to hold inmemory data of bootstrap during build.
pub struct BootstrapContext {
    /// This build has a parent bootstrap.
    pub layered: bool,
    /// Cache node index for hardlinks, HashMap<(layer_index, real_inode, dev), Vec<index>>.
    pub inode_map: HashMap<(u16, Inode, u64), Vec<u64>>,
    /// Store all nodes in ascendant ordor, indexed by (node.index - 1).
    pub nodes: Vec<Node>,
    /// Current position to write in f_bootstrap
    pub offset: u64,
    pub writer: Box<dyn RafsIoWrite>,
    /// Not fully used blocks
    pub v6_available_blocks: Vec<VecDeque<u64>>,
}

impl BootstrapContext {
    pub fn new(storage: Option<ArtifactStorage>, layered: bool, fifo: bool) -> Result<Self> {
        let writer = if let Some(storage) = storage {
            Box::new(ArtifactFileWriter(ArtifactWriter::new(storage, fifo)?))
                as Box<dyn RafsIoWrite>
        } else {
            Box::new(ArtifactMemoryWriter(Cursor::new(Vec::new()))) as Box<dyn RafsIoWrite>
        };
        Ok(Self {
            layered,
            inode_map: HashMap::new(),
            nodes: Vec::new(),
            offset: EROFS_BLOCK_SIZE,
            writer,
            v6_available_blocks: vec![
                VecDeque::new();
                EROFS_BLOCK_SIZE as usize / EROFS_INODE_SLOT_SIZE
            ],
        })
    }

    pub fn align_offset(&mut self, align_size: u64) {
        if self.offset % align_size > 0 {
            self.offset = div_round_up(self.offset, align_size) * align_size;
        }
    }

    // Only used to allocate space for metadata(inode / inode + inline data).
    // Try to find an used block with no less than `size` space left.
    // If found it, return the offset where we can store data.
    // If not, return 0.
    pub fn allocate_available_block(&mut self, size: u64) -> u64 {
        if size >= EROFS_BLOCK_SIZE {
            return 0;
        }

        let min_idx = div_round_up(size, EROFS_INODE_SLOT_SIZE as u64) as usize;
        let max_idx = div_round_up(EROFS_BLOCK_SIZE, EROFS_INODE_SLOT_SIZE as u64) as usize;

        for idx in min_idx..max_idx {
            let blocks = &mut self.v6_available_blocks[idx];
            if let Some(mut offset) = blocks.pop_front() {
                offset += EROFS_BLOCK_SIZE - (idx * EROFS_INODE_SLOT_SIZE) as u64;
                self.append_available_block(offset + (min_idx * EROFS_INODE_SLOT_SIZE) as u64);
                return offset;
            }
        }

        0
    }

    // Append the block that `offset` belongs to corresponding deque.
    pub fn append_available_block(&mut self, offset: u64) {
        if offset % EROFS_BLOCK_SIZE != 0 {
            let avail = EROFS_BLOCK_SIZE - offset % EROFS_BLOCK_SIZE;
            let idx = avail as usize / EROFS_INODE_SLOT_SIZE;
            self.v6_available_blocks[idx].push_back(round_down_4k(offset));
        }
    }
}

/// BootstrapManager is used to hold the parent bootstrap reader and create
/// new bootstrap context.
pub struct BootstrapManager {
    /// Parent bootstrap file reader.
    pub f_parent_bootstrap: Option<RafsIoReader>,
    pub bootstrap_storage: Option<ArtifactStorage>,
}

impl BootstrapManager {
    pub fn new(
        bootstrap_storage: Option<ArtifactStorage>,
        f_parent_bootstrap: Option<RafsIoReader>,
    ) -> Self {
        Self {
            f_parent_bootstrap,
            bootstrap_storage,
        }
    }

    pub fn create_ctx(&self, fifo: bool) -> Result<BootstrapContext> {
        BootstrapContext::new(
            self.bootstrap_storage.clone(),
            self.f_parent_bootstrap.is_some(),
            fifo,
        )
    }
}

pub struct BuildContext {
    /// Blob id (user specified or sha256(blob)).
    pub blob_id: String,

    /// When filling local blobcache file, chunks are arranged as per the
    /// `decompress_offset` within chunk info. Therefore, provide a new flag
    /// to image tool thus to align chunks in blob with 4k size.
    pub aligned_chunk: bool,
    /// Add a offset for compressed blob.
    pub blob_offset: u64,
    /// Blob chunk compress flag.
    pub compressor: compress::Algorithm,
    /// Inode and chunk digest algorithm flag.
    pub digester: digest::Algorithm,
    /// Save host uid gid in each inode.
    pub explicit_uidgid: bool,
    /// whiteout spec: overlayfs or oci
    pub whiteout_spec: WhiteoutSpec,
    /// Chunk slice size.
    pub chunk_size: u32,
    /// Version number of output metadata and data blob.
    pub fs_version: RafsVersion,

    /// Format conversion type.
    pub conversion_type: ConversionType,
    /// Path of source to build the image from:
    /// - Directory: `source_path` should be a directory path
    /// - StargzIndex: `source_path` should be a stargz index json file path
    pub source_path: PathBuf,

    /// Track file/chunk prefetch state.
    pub prefetch: Prefetch,

    /// Storage writing blob to single file or a directory.
    pub blob_storage: Option<ArtifactStorage>,
    pub blob_meta_storage: Option<ArtifactStorage>,
    pub blob_zran_generator: Option<Mutex<ZranContextGenerator<File>>>,
    pub blob_meta_features: u32,
    pub inline_bootstrap: bool,
    pub has_xattr: bool,
}

impl BuildContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        blob_id: String,
        aligned_chunk: bool,
        blob_offset: u64,
        compressor: compress::Algorithm,
        digester: digest::Algorithm,
        explicit_uidgid: bool,
        whiteout_spec: WhiteoutSpec,
        source_type: ConversionType,
        source_path: PathBuf,
        prefetch: Prefetch,
        blob_storage: Option<ArtifactStorage>,
        blob_meta_storage: Option<ArtifactStorage>,
        inline_bootstrap: bool,
    ) -> Self {
        BuildContext {
            blob_id,
            aligned_chunk,
            blob_offset,
            compressor,
            digester,
            explicit_uidgid,
            whiteout_spec,

            chunk_size: RAFS_DEFAULT_CHUNK_SIZE as u32,
            fs_version: RafsVersion::default(),

            conversion_type: source_type,
            source_path,

            prefetch,
            blob_storage,
            blob_meta_storage,
            blob_zran_generator: None,
            blob_meta_features: 0,
            inline_bootstrap,
            has_xattr: false,
        }
    }

    pub fn set_fs_version(&mut self, fs_version: RafsVersion) {
        self.fs_version = fs_version;
    }

    pub fn set_chunk_size(&mut self, chunk_size: u32) {
        self.chunk_size = chunk_size;
    }
}

impl Default for BuildContext {
    fn default() -> Self {
        Self {
            blob_id: String::new(),
            aligned_chunk: false,
            blob_offset: 0,
            compressor: compress::Algorithm::default(),
            digester: digest::Algorithm::default(),
            explicit_uidgid: true,
            whiteout_spec: WhiteoutSpec::default(),

            chunk_size: RAFS_DEFAULT_CHUNK_SIZE as u32,
            fs_version: RafsVersion::default(),

            conversion_type: ConversionType::default(),
            source_path: PathBuf::new(),

            prefetch: Prefetch::default(),
            blob_storage: None,
            blob_meta_storage: None,
            blob_zran_generator: None,
            blob_meta_features: 0,
            has_xattr: true,
            inline_bootstrap: false,
        }
    }
}

/// BuildOutput represents the output in this build.
#[derive(Default, Debug, Clone)]
pub struct BuildOutput {
    /// Blob ids in the blob table of bootstrap.
    pub blobs: Vec<String>,
    /// The size of output blob in this build.
    pub blob_size: Option<u64>,
    /// File path for the metadata blob.
    pub bootstrap_path: Option<String>,
}

impl BuildOutput {
    pub fn new(
        blob_mgr: &BlobManager,
        bootstrap_storage: &Option<ArtifactStorage>,
    ) -> Result<BuildOutput> {
        let blobs = blob_mgr.get_blob_ids();
        let blob_size = blob_mgr.get_last_blob().map(|b| b.compressed_blob_size);
        let bootstrap_path = if let Some(ArtifactStorage::SingleFile(p)) = bootstrap_storage {
            Some(p.display().to_string())
        } else {
            None
        };

        Ok(Self {
            blobs,
            blob_size,
            bootstrap_path,
        })
    }
}
