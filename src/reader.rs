//! Opening one backup file and parsing everything this crate needs from it.
//!
//! The read order is fixed by the format and mirrors the reference SDK's
//! `readBackupFile`: footer, root list, `$JSON`, then the per-disk and per-partition lists
//! starting at `_header.index_file_position`. The disk and partition regions carry no
//! self-describing counts, so the JSON has to be parsed first to know how many of each to
//! expect.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;

use crate::block::{self, BlockList};
use crate::index::PartitionIndex;
use crate::json::{self, Header};

/// One partition's metadata list and the index that ends it.
#[derive(Debug, Clone)]
pub struct Partition {
    /// Every block of this partition's list, `$INDEX` last.
    pub blocks: BlockList,
    pub index: PartitionIndex,
}

impl Partition {
    /// The byte range of this partition's list up to but not including `$INDEX`.
    ///
    /// A consolidated output copies that range verbatim and emits a fresh `$INDEX` after
    /// it. The copy is safe even for an encrypted set, because metadata uses AES-ECB with
    /// no initialization vector and so has no positional dependence.
    pub fn span_before_index(&self) -> (u64, u64) {
        let start = self.blocks.blocks[0].offset;
        (start, self.blocks.last().offset)
    }
}

/// One disk's metadata list, and its partitions in array order.
#[derive(Debug, Clone)]
pub struct Disk {
    pub blocks: BlockList,
    pub partitions: Vec<Partition>,
}

/// A parsed backup file.
#[derive(Debug, Clone)]
pub struct BackupFile {
    pub path: PathBuf,
    pub size: u64,
    /// The `$JSON` payload exactly as stored. Kept so the canonical round-trip gate has
    /// something to compare against.
    pub json_raw: Vec<u8>,
    pub json: Value,
    pub header: Header,
    /// Offset of the root metadata list, as the footer gives it.
    pub root_at: u64,
    pub root_list: BlockList,
    /// Empty for a split continuation file, which carries data blocks and no index.
    pub disks: Vec<Disk>,
}

impl BackupFile {
    /// The number of disks and partitions the JSON declares.
    fn shape(json: &Value) -> Result<Vec<usize>> {
        let disks = json
            .get("disks")
            .and_then(Value::as_array)
            .context("the $JSON block has no disks array")?;
        disks
            .iter()
            .enumerate()
            .map(|(i, disk)| {
                disk.get("partitions")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .with_context(|| format!("disks[{i}] has no partitions array"))
            })
            .collect()
    }

    /// Open and parse a backup file.
    ///
    /// `load_index` mirrors the reference reader's flag of the same name. Set it false to
    /// read only identity and layout, which is what set discovery needs when it is
    /// deciding whether a file belongs to the set at all.
    pub fn open(path: impl AsRef<Path>, load_index: bool) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut reader = BufReader::new(file);
        Self::parse(path.to_path_buf(), &mut reader, load_index)
            .with_context(|| format!("reading {}", path.display()))
    }

    fn parse<R: Read + Seek>(path: PathBuf, reader: &mut R, load_index: bool) -> Result<Self> {
        let size = reader.seek(SeekFrom::End(0))?;
        let root_at = block::read_footer(reader)?;

        let root_list = block::walk_list(reader, root_at).context("walking the root block list")?;
        let json_block = root_list
            .find(block::JSON)
            .context("the root block list holds no $JSON block")?;
        ensure!(
            !json_block.header.flags.encryption,
            "the $JSON block is marked encrypted, which the format does not allow"
        );
        ensure!(
            !json_block.header.flags.compression,
            "the $JSON block is compressed; this build cannot read it yet"
        );

        let json_raw = block::read_payload(reader, json_block)?;
        let json = json::parse(&json_raw)?;
        let header = Header::from_value(&json)?;

        let mut disks = Vec::new();
        if load_index && !header.split_file {
            let shape = Self::shape(&json)?;
            let mut at = header.index_file_position;
            ensure!(
                at < root_at,
                "index_file_position {at} is not below the root list at {root_at}; \
                 the metadata region is not where this format puts it"
            );
            for (d, partition_count) in shape.into_iter().enumerate() {
                let blocks = block::walk_list(reader, at)
                    .with_context(|| format!("walking the metadata list of disk {d}"))?;
                at = blocks.end;
                let mut partitions = Vec::with_capacity(partition_count);
                for p in 0..partition_count {
                    let blocks = block::walk_list(reader, at).with_context(|| {
                        format!("walking the metadata list of disk {d} partition {p}")
                    })?;
                    at = blocks.end;
                    let index_block = blocks.last();
                    if !index_block.header.is(block::INDEX) {
                        bail!(
                            "disk {d} partition {p} metadata ends with {} rather than $INDEX",
                            index_block.header.name_str()
                        );
                    }
                    // The reference reader re-reads this payload raw after a rewind, which
                    // only works when it is stored plain. Every corpus file agrees. Refuse
                    // anything else rather than guess.
                    ensure!(
                        !index_block.header.flags.compression
                            && !index_block.header.flags.encryption,
                        "disk {d} partition {p} has a compressed or encrypted $INDEX block, \
                         which this crate's model of the format says cannot happen"
                    );
                    let raw = block::read_payload(reader, index_block)?;
                    let index = PartitionIndex::parse(&raw, header.delta_index)
                        .with_context(|| format!("parsing $INDEX of disk {d} partition {p}"))?;
                    partitions.push(Partition { blocks, index });
                }
                disks.push(Disk { blocks, partitions });
            }
            ensure!(
                at == root_at,
                "the metadata region ends at {at} but the root list starts at {root_at}; \
                 the block walk drifted by {} bytes",
                at.abs_diff(root_at)
            );
        }

        Ok(Self {
            path,
            size,
            json_raw,
            json,
            header,
            root_at,
            root_list,
            disks,
        })
    }

    /// Bytes between the end of the root list and the end of the file. A well-formed file
    /// reports exactly [`block::FOOTER_LEN`].
    pub fn trailing_bytes(&self) -> u64 {
        self.size.saturating_sub(self.root_list.end)
    }

    /// Make sure that the block walk landed exactly on the footer.
    ///
    /// This one assertion catches most framing bugs, because any drift anywhere in the
    /// metadata region shows up here as a wrong tail length.
    pub fn check_framing(&self) -> Result<()> {
        let tail = self.trailing_bytes();
        ensure!(
            tail == block::FOOTER_LEN,
            "{}: {tail} bytes follow the root block list, expected {}; \
             the metadata walk drifted",
            self.path.display(),
            block::FOOTER_LEN
        );
        Ok(())
    }

    /// The highest offset one past a block that this file stores itself.
    ///
    /// On every corpus file this equals `index_file_position` exactly, so the data region
    /// is contiguous from offset 0 and carries no padding.
    pub fn own_data_end(&self) -> u64 {
        let mut end = 0u64;
        for disk in &self.disks {
            for part in &disk.partitions {
                let mut consider = |e: &crate::index::DataBlockIndexElement| {
                    if e.is_hole() || e.file_number != self.header.file_number {
                        return;
                    }
                    let stop = e.file_position.max(0) as u64 + u64::from(e.block_length);
                    end = end.max(stop);
                };
                for e in &part.index.reserved {
                    consider(e);
                }
                match &part.index.blocks {
                    crate::index::Blocks::Full(v) => v.iter().for_each(&mut consider),
                    crate::index::Blocks::Delta(v) => {
                        v.iter().for_each(|d| consider(&d.element));
                    }
                }
            }
        }
        end
    }
}
