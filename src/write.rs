//! Writing a consolidated file: the data region, then the metadata region.
//!
//! This is the forward, append-only pass. Blocks are copied byte for byte: nothing is
//! decompressed, nothing is re-encrypted, and `block_length` and `md5_hash` are carried
//! through untouched. Only `file_position` and `file_number` change, because only those two
//! describe where the bytes live rather than what they are.
//!
//! That is sound because the per-block encryption initialization vector is derived from the
//! image id, the disk number, the partition number and the logical block index. A merge
//! preserves all four. See `scratch/format-notes.md`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde_json::{Map, Value};

use crate::block::{self, align_up, MetadataBlockHeader};
use crate::index::{Blocks, DataBlockIndexElement, DeltaDataBlock, PartitionIndex};
use crate::json;
use crate::plan::{Action, MergeKind, MergePlan, PartitionPlan};
use crate::reader::{BackupFile, Disk};
use crate::set::BackupSet;

/// Somewhere to read source blocks from.
///
/// Abstracted so the copy loop can be tested without a backup set on disk. There are two
/// implementations: [`SourceFiles`] over real files, and a fake one in the tests.
pub trait BlockSource {
    /// Read exactly `length` bytes at `position` from the file that `file_number` names.
    fn read_block(&mut self, file_number: u16, position: i64, length: u32) -> Result<Vec<u8>>;
}

/// The open source files of a merge, keyed by every file number they answer for.
pub struct SourceFiles {
    handles: HashMap<u16, BufReader<File>>,
}

impl SourceFiles {
    /// Open every file the plan reads from, read-only.
    ///
    /// A member answers for its own file number and for every number it absorbed, so two
    /// keys can name one file. Each path is opened once.
    pub fn open(set: &BackupSet, plan: &MergePlan) -> Result<Self> {
        let mut handles = HashMap::new();
        for number in plan.absorbed.iter().copied() {
            let owner = set
                .owner(number)
                .with_context(|| format!("no member of the set owns file number {number}"))?;
            let path: &PathBuf = &owner.path;
            // One reader per file number, even when two numbers name the same file. Each
            // keeps its own cursor, which keeps the copy loop free of seek bookkeeping.
            let file = File::open(path)
                .with_context(|| format!("opening {} to read blocks", path.display()))?;
            handles.insert(number, BufReader::new(file));
        }
        Ok(Self { handles })
    }
}

impl BlockSource for SourceFiles {
    fn read_block(&mut self, file_number: u16, position: i64, length: u32) -> Result<Vec<u8>> {
        let reader = self
            .handles
            .get_mut(&file_number)
            .with_context(|| format!("no open handle for file number {file_number}"))?;
        ensure!(position >= 0, "block position {position} is negative");
        reader
            .seek(SeekFrom::Start(position as u64))
            .with_context(|| format!("seeking to {position} in file {file_number}"))?;
        let mut buf = vec![0u8; length as usize];
        reader.read_exact(&mut buf).with_context(|| {
            format!("reading {length} bytes at {position} from file {file_number}")
        })?;
        Ok(buf)
    }
}

/// One partition's index, after the data region has been written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenPartition {
    pub disk: usize,
    pub partition: usize,
    pub index: PartitionIndex,
}

/// The result of writing the data region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataRegion {
    pub partitions: Vec<WrittenPartition>,
    /// Bytes of block payload written, before padding.
    pub payload_bytes: u64,
    /// Padded end of the region. This becomes `_header.index_file_position`.
    pub end: u64,
    /// How many copies the duplicate map avoided.
    pub duplicates_avoided: usize,
}

/// Write the data region and return the index that describes it.
///
/// `out` receives the bytes and must start empty, because positions are recorded as offsets
/// from zero. `out_file_number` is the number the output claims, and every copied entry is
/// rewritten to it.
pub fn write_data_region<W: Write, S: BlockSource>(
    plan: &MergePlan,
    source: &mut S,
    out: &mut W,
    out_file_number: u16,
) -> Result<DataRegion> {
    let mut at = 0u64;
    let mut seen: HashMap<(u16, i64, u32), i64> = HashMap::new();
    let mut duplicates_avoided = 0usize;
    let mut partitions = Vec::with_capacity(plan.partitions.len());

    for part in &plan.partitions {
        let reserved = copy_actions(
            &part.reserved,
            source,
            out,
            out_file_number,
            &mut at,
            &mut seen,
            &mut duplicates_avoided,
        )
        .with_context(|| {
            format!(
                "copying reserved sectors of disk {} partition {}",
                part.disk, part.partition
            )
        })?;

        let blocks = copy_actions(
            &part.blocks,
            source,
            out,
            out_file_number,
            &mut at,
            &mut seen,
            &mut duplicates_avoided,
        )
        .with_context(|| {
            format!(
                "copying data blocks of disk {} partition {}",
                part.disk, part.partition
            )
        })?;

        partitions.push(WrittenPartition {
            disk: part.disk,
            partition: part.partition,
            index: PartitionIndex {
                reserved,
                blocks: shape(plan, part, blocks)?,
            },
        });
    }

    let payload_bytes = at;
    // The metadata region starts on the next 4096-byte boundary. Measured on every file of
    // both test sets. An uncompressed set hides this, because its blocks land on the
    // boundary anyway and the gap reads as zero.
    let end = align_up(at);
    let padding = end - at;
    if padding > 0 {
        write_zeros(out, padding)?;
    }

    Ok(DataRegion {
        partitions,
        payload_bytes,
        end,
        duplicates_avoided,
    })
}

/// Copy one array of actions and return the index entries that describe the result.
#[allow(clippy::too_many_arguments)] // The alternative is a struct used in exactly one place.
fn copy_actions<W: Write, S: BlockSource>(
    actions: &[Action],
    source: &mut S,
    out: &mut W,
    out_file_number: u16,
    at: &mut u64,
    seen: &mut HashMap<(u16, i64, u32), i64>,
    duplicates_avoided: &mut usize,
) -> Result<Vec<DataBlockIndexElement>> {
    let mut entries = Vec::with_capacity(actions.len());
    for action in actions {
        let entry = match action {
            // A hole is written as all zeros. The reference restore skips it.
            Action::Hole => DataBlockIndexElement::default(),
            // The owning file stays on disk, so the reference is still good.
            Action::Keep(e) => *e,
            Action::Copy(e) => {
                let key = (e.file_number, e.file_position, e.block_length);
                let position = match seen.get(&key) {
                    Some(already) => {
                        *duplicates_avoided += 1;
                        *already
                    }
                    None => {
                        let bytes =
                            source.read_block(e.file_number, e.file_position, e.block_length)?;
                        ensure!(
                            bytes.len() as u32 == e.block_length,
                            "source returned {} bytes for a block of {}",
                            bytes.len(),
                            e.block_length
                        );
                        let position = i64::try_from(*at).context("output offset overflows")?;
                        out.write_all(&bytes)?;
                        *at += u64::from(e.block_length);
                        seen.insert(key, position);
                        position
                    }
                };
                DataBlockIndexElement {
                    file_position: position,
                    // Untouched. The hash covers the plaintext, which does not change, and
                    // the length is the stored length, which does not change either.
                    md5_hash: e.md5_hash,
                    block_length: e.block_length,
                    file_number: out_file_number,
                }
            }
        };
        entries.push(entry);
    }
    Ok(entries)
}

/// Put the block entries into the form the output's index uses.
fn shape(
    plan: &MergePlan,
    part: &PartitionPlan,
    entries: Vec<DataBlockIndexElement>,
) -> Result<Blocks> {
    if !plan.kind.delta_index() {
        return Ok(Blocks::Full(entries));
    }
    ensure!(
        part.positions.len() == entries.len(),
        "disk {} partition {} has {} delta entries but {} positions",
        part.disk,
        part.partition,
        entries.len(),
        part.positions.len()
    );
    Ok(Blocks::Delta(
        entries
            .into_iter()
            .zip(part.positions.iter().copied())
            .map(|(element, block_index)| DeltaDataBlock {
                element,
                block_index,
            })
            .collect(),
    ))
}

fn write_zeros<W: Write>(out: &mut W, mut count: u64) -> Result<()> {
    const CHUNK: usize = 8192;
    let zeros = [0u8; CHUNK];
    while count > 0 {
        let n = count.min(CHUNK as u64) as usize;
        out.write_all(&zeros[..n])?;
        count -= n as u64;
    }
    Ok(())
}

/// Write the metadata region: the per-disk and per-partition lists, and a fresh `$INDEX`.
///
/// Every block of the To file up to each partition's `$INDEX` is copied byte for byte. That
/// covers `$TRACK0`, `$EPT`, `$BITMAP` and any block name this crate does not know. The copy
/// is safe even for an encrypted set, because metadata uses AES-ECB with no initialization
/// vector and so does not depend on where it sits.
///
/// Each `$INDEX` is rebuilt from `data`, stored plain, and carries the last-block flag,
/// because it ends its partition list. It is stored plain because the reference reader
/// re-reads that payload raw, which only works when nothing was done to it.
///
/// `disks` comes from the To file, and `data` must describe the same disks and partitions in
/// the same order. The region starts at [`DataRegion::end`], so `out` must already hold the
/// data region. The returned offset is one past the region, which is where the root list
/// starts and what the footer records.
pub fn write_metadata_region<W: Write, S: BlockSource>(
    disks: &[Disk],
    data: &DataRegion,
    source: &mut S,
    out: &mut W,
    to_file_number: u16,
) -> Result<u64> {
    let expected: usize = disks.iter().map(|d| d.partitions.len()).sum();
    ensure!(
        data.partitions.len() == expected,
        "the data region describes {} partitions but the To file holds {expected}",
        data.partitions.len()
    );

    let mut at = data.end;
    let mut next = 0usize;
    for (d, disk) in disks.iter().enumerate() {
        let start = disk.blocks.blocks[0].offset;
        let length = disk.blocks.end - start;
        copy_range(source, out, to_file_number, start, length)
            .with_context(|| format!("copying the metadata list of disk {d}"))?;
        at += length;

        for (p, part) in disk.partitions.iter().enumerate() {
            let (start, index_at) = part.span_before_index();
            let length = index_at - start;
            copy_range(source, out, to_file_number, start, length)
                .with_context(|| format!("copying the metadata list of disk {d} partition {p}"))?;
            at += length;

            let written = &data.partitions[next];
            ensure!(
                written.disk == d && written.partition == p,
                "the data region holds disk {} partition {} where the To file holds \
                 disk {d} partition {p}",
                written.disk,
                written.partition
            );
            next += 1;

            let payload = written.index.to_bytes();
            let header = MetadataBlockHeader::for_payload(block::INDEX, &payload, true)?;
            out.write_all(&header.to_bytes())?;
            out.write_all(&payload)?;
            at += (block::HEADER_LEN + payload.len()) as u64;
        }
    }

    Ok(at)
}

/// Copy a byte range of one source file straight through.
///
/// The read is chunked, because the `$BITMAP` of a large disk runs to tens of megabytes and
/// there is no reason to hold one in memory whole.
fn copy_range<W: Write, S: BlockSource>(
    source: &mut S,
    out: &mut W,
    file_number: u16,
    start: u64,
    length: u64,
) -> Result<()> {
    const CHUNK: u64 = 4 * 1024 * 1024;
    let mut done = 0u64;
    while done < length {
        let take = u32::try_from((length - done).min(CHUNK)).expect("CHUNK fits in a u32");
        let position = i64::try_from(start + done).context("metadata offset overflows")?;
        let bytes = source.read_block(file_number, position, take)?;
        ensure!(
            bytes.len() as u32 == take,
            "source returned {} bytes for a {take}-byte range at {position}",
            bytes.len()
        );
        out.write_all(&bytes)?;
        done += u64::from(take);
    }
    Ok(())
}

/// Patch the metadata document of the To file and return it in canonical form.
///
/// The round-trip gate runs first. If re-serializing the untouched document does not
/// reproduce the source bytes, this crate cannot rewrite the metadata of this file, and the
/// run stops before anything is written.
///
/// `output_name` is the file name of the output with no directory. Every path the document
/// carries is replaced, because a real file records absolute paths from the machine that
/// took the backup, such as `C:\Users\<name>\Desktop\...`, and the output must not carry
/// them. Volume device paths such as `\\?\Volume{...}` stay: they describe the imaged
/// volume, not a directory on that machine.
pub fn patch_document(
    set: &BackupSet,
    plan: &MergePlan,
    index_file_position: u64,
    output_name: &str,
) -> Result<Vec<u8>> {
    let to = set
        .owner(plan.to)
        .with_context(|| format!("no member of the set owns file number {}", plan.to))?;
    json::check_round_trip(&to.json, &to.json_raw)
        .with_context(|| format!("the $JSON block of {}", to.path.display()))?;

    let mut doc = to.json.clone();
    patch_header(&mut doc, plan, index_file_position)?;
    patch_partitions(&mut doc, plan, output_name)?;
    patch_auxiliary_data(&mut doc, plan, output_name)?;
    if plan.kind == MergeKind::SyntheticFull {
        patch_disk_sizes(&mut doc, &set.base()?.json)?;
    }

    json::canonical(&doc)
}

/// Record what the output is: its number, what it absorbed, and the shape of its index.
fn patch_header(doc: &mut Value, plan: &MergePlan, index_file_position: u64) -> Result<()> {
    let header = object_mut(doc, "_header")?;
    header.insert("file_number".into(), plan.out_file_number.into());
    header.insert("increment_number".into(), plan.out_increment_number.into());
    // Absent from every unconsolidated file, so this inserts the key rather than replacing
    // it. The output's own number is not in the list: that number names the file itself.
    let merged: Vec<u16> = plan
        .redundant_file_numbers()
        .into_iter()
        .filter(|n| *n != plan.out_file_number)
        .collect();
    header.insert("merged_files".into(), merged.into());
    header.insert("index_file_position".into(), index_file_position.into());
    header.insert("delta_index".into(), plan.kind.delta_index().into());
    // The output is one file. A split continuation is refused before the write starts.
    header.insert("split_file".into(), false.into());
    // The name of the machine that took the backup, for example DESKTOP-EXAMPLE. The tool
    // that wrote the output did not run there, so the key is kept and its value is cleared.
    if header.contains_key("netbios_name") {
        header.insert("netbios_name".into(), "".into());
    }
    if plan.kind == MergeKind::SyntheticFull {
        // The output carries a complete index, so it is a Full whatever the backup
        // definition called the To file.
        header.insert("backup_type".into(), "full".into());
    }
    Ok(())
}

/// Rewrite the per-partition file history so that it names files rather than paths.
///
/// An absorbed number now lives in the output, so its entry names the output. Any other
/// entry keeps its own file name with the directory removed. The name is a locator hint:
/// the reference reader resolves it against the directory it was given.
fn patch_partitions(doc: &mut Value, plan: &MergePlan, output_name: &str) -> Result<()> {
    let disks = doc
        .get_mut("disks")
        .and_then(Value::as_array_mut)
        .context("the $JSON document has no disks array")?;

    for (d, disk) in disks.iter_mut().enumerate() {
        let partitions = disk
            .get_mut("partitions")
            .and_then(Value::as_array_mut)
            .with_context(|| format!("disks[{d}] has no partitions array"))?;

        for (p, part) in partitions.iter_mut().enumerate() {
            let header = object_mut(part, "_header")
                .with_context(|| format!("disk {d} partition {p} of the $JSON document"))?;
            let count = {
                let Some(history) = header.get_mut("file_history").and_then(Value::as_array_mut)
                else {
                    continue;
                };
                for entry in history.iter_mut() {
                    let number = entry
                        .get("file_number")
                        .and_then(Value::as_u64)
                        .and_then(|n| u16::try_from(n).ok());
                    let recorded = entry.get("file_name").and_then(Value::as_str).unwrap_or("");
                    let name = match number {
                        Some(n) if plan.absorbed.contains(&n) => output_name.to_string(),
                        _ => bare_name(recorded),
                    };
                    if let Some(entry) = entry.as_object_mut() {
                        entry.insert("file_name".into(), name.into());
                    }
                }
                history.len()
            };
            header.insert("file_history_count".into(), count.into());
        }
    }
    Ok(())
}

/// Record the merge in the backup definition, and drop the paths it carries.
fn patch_auxiliary_data(doc: &mut Value, plan: &MergePlan, output_name: &str) -> Result<()> {
    let aux = object_mut(doc, "_auxiliary_data")?;
    // The full path of the output, which the tool that wrote it recorded.
    if aux.contains_key("destination") {
        aux.insert("destination".into(), output_name.into());
    }

    let definition = aux
        .get_mut("backup_definition")
        .and_then(Value::as_object_mut)
        .context("_auxiliary_data has no backup_definition object")?;
    definition.insert(
        "consolidation_type".into(),
        plan.kind.consolidation_type().into(),
    );
    if definition.contains_key("filename") {
        definition.insert("filename".into(), output_name.into());
    }
    // The definition file lives on the machine that took the backup, so only its name is
    // kept.
    let bare = definition
        .get("backup_definition_file")
        .and_then(Value::as_str)
        .map(bare_name);
    if let Some(bare) = bare {
        definition.insert("backup_definition_file".into(), bare.into());
    }
    Ok(())
}

/// Take `disk_size` from the file that carries the full index.
///
/// A Full records the true device size. An Incremental records the CHS product, which
/// rounds down to a whole cylinder, so the To file's value is wrong for a synthetic Full.
/// An extraction sized from the wrong member produces a false result in either direction.
fn patch_disk_sizes(doc: &mut Value, base: &Value) -> Result<()> {
    let sizes: Vec<Value> = base
        .get("disks")
        .and_then(Value::as_array)
        .context("the file with the full index has no disks array")?
        .iter()
        .enumerate()
        .map(|(d, disk)| {
            disk.get("_geometry")
                .and_then(|g| g.get("disk_size"))
                .cloned()
                .with_context(|| {
                    format!("disks[{d}] of the file with the full index has no _geometry.disk_size")
                })
        })
        .collect::<Result<_>>()?;

    let disks = doc
        .get_mut("disks")
        .and_then(Value::as_array_mut)
        .context("the $JSON document has no disks array")?;
    ensure!(
        disks.len() == sizes.len(),
        "the To file holds {} disks but the file with the full index holds {}",
        disks.len(),
        sizes.len()
    );
    for (disk, size) in disks.iter_mut().zip(sizes) {
        object_mut(disk, "_geometry")?.insert("disk_size".into(), size);
    }
    Ok(())
}

fn object_mut<'a>(value: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>> {
    value
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .with_context(|| format!("the $JSON document has no {key} object"))
}

/// The file name with any directory removed, for both Windows and Unix separators.
fn bare_name(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

/// A finished output file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub data: DataRegion,
    /// Offset of the root block list, which is what the footer records.
    pub root_at: u64,
    /// Size of the finished file.
    pub size: u64,
}

/// Write a whole consolidated file: data region, metadata region, root list, footer.
///
/// `out` must start empty and receives the file from offset zero. `output_name` is the file
/// name the output will carry, with no directory, and it is written into the document.
///
/// Nothing here opens the destination or renames anything. The caller owns the commit
/// sequence, so this function can be tested against a buffer.
pub fn write_output<W: Write, S: BlockSource>(
    set: &BackupSet,
    plan: &MergePlan,
    source: &mut S,
    out: &mut W,
    output_name: &str,
) -> Result<Written> {
    let to = set
        .owner(plan.to)
        .with_context(|| format!("no member of the set owns file number {}", plan.to))?;

    let data = write_data_region(plan, source, out, plan.out_file_number)?;
    let root_at = write_metadata_region(&to.disks, &data, source, out, plan.to)?;

    // The root list: the patched document, then `$AUXDATA` if the To file carries one.
    let document = patch_document(set, plan, data.end, output_name)?;
    let aux = to.root_list.find(block::AUXDATA);
    // Stored plain. A compressed source document is decompressed on the way in, and the
    // reference reader decompresses only when the flag says to.
    let header = MetadataBlockHeader::for_payload(block::JSON, &document, aux.is_none())?;
    out.write_all(&header.to_bytes())?;
    out.write_all(&document)?;
    let mut at = root_at + (block::HEADER_LEN + document.len()) as u64;

    if let Some(aux) = aux {
        // 32 bytes in every corpus file. This crate does not model what is in it, so the
        // block is copied with its own header, flags and all. It ends the list.
        let length = block::HEADER_LEN as u64 + u64::from(aux.header.block_length);
        copy_range(source, out, plan.to, aux.offset, length)
            .context("copying the $AUXDATA block")?;
        at += length;
    }

    out.write_all(&block::footer_bytes(root_at))?;

    Ok(Written {
        data,
        root_at,
        size: at + block::FOOTER_LEN,
    })
}

/// Read a finished output back and make sure that it is what the plan describes.
///
/// This runs before the output is renamed into place, so a framing mistake shows up now
/// rather than in a restore months later. The read uses this crate's own reader, which
/// proves the file parses but not that it is correct: only an extraction comparison against
/// the independent reference extractor proves that.
pub fn check_output(path: &Path, plan: &MergePlan) -> Result<()> {
    let file = BackupFile::open(path, true)?;
    file.check_framing()?;

    ensure!(
        file.header.file_number == plan.out_file_number,
        "the output claims file number {} rather than {}",
        file.header.file_number,
        plan.out_file_number
    );
    ensure!(
        file.header.delta_index == plan.kind.delta_index(),
        "the output claims delta_index {} rather than {}",
        file.header.delta_index,
        plan.kind.delta_index()
    );

    let data_end = file.header.index_file_position;
    for (d, disk) in file.disks.iter().enumerate() {
        for (p, part) in disk.partitions.iter().enumerate() {
            let check = |entry: &DataBlockIndexElement| -> Result<()> {
                if entry.is_hole() {
                    return Ok(());
                }
                ensure!(
                    entry.file_number == plan.out_file_number
                        || !plan.absorbed.contains(&entry.file_number),
                    "disk {d} partition {p} still references file number {}, \
                     which the merge absorbed",
                    entry.file_number
                );
                if entry.file_number == plan.out_file_number {
                    let start = u64::try_from(entry.file_position)
                        .context("an index entry of the output has a negative position")?;
                    let end = start + u64::from(entry.block_length);
                    ensure!(
                        end <= data_end,
                        "disk {d} partition {p} has a block at {start} of {} bytes, \
                         which runs past the data region at {data_end}",
                        entry.block_length
                    );
                }
                Ok(())
            };

            for entry in &part.index.reserved {
                check(entry)?;
            }
            match &part.index.blocks {
                Blocks::Full(entries) => {
                    for entry in entries {
                        check(entry)?;
                    }
                }
                Blocks::Delta(entries) => {
                    for delta in entries {
                        check(&delta.element)?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{MergeKind, PartitionPlan};
    use crate::reader::Partition;
    use std::collections::BTreeSet;
    use std::io::Cursor;

    /// A source whose bytes are derived from the request, so a copy can be tested without
    /// any file on disk.
    struct FakeSource {
        reads: Vec<(u16, i64, u32)>,
    }

    impl FakeSource {
        fn new() -> Self {
            Self { reads: Vec::new() }
        }

        /// The bytes a given block is expected to hold.
        fn expected(file_number: u16, position: i64, length: u32) -> Vec<u8> {
            (0..length)
                .map(|i| (file_number as u8) ^ (position as u8) ^ (i as u8))
                .collect()
        }
    }

    impl BlockSource for FakeSource {
        fn read_block(&mut self, file_number: u16, position: i64, length: u32) -> Result<Vec<u8>> {
            self.reads.push((file_number, position, length));
            Ok(Self::expected(file_number, position, length))
        }
    }

    fn element(file_number: u16, position: i64, length: u32) -> DataBlockIndexElement {
        DataBlockIndexElement {
            file_position: position,
            md5_hash: [file_number as u8; 16],
            block_length: length,
            file_number,
        }
    }

    fn plan_with(kind: MergeKind, blocks: Vec<Action>, positions: Vec<u32>) -> MergePlan {
        plan_with_reserved(kind, Vec::new(), blocks, positions)
    }

    fn plan_with_reserved(
        kind: MergeKind,
        reserved: Vec<Action>,
        blocks: Vec<Action>,
        positions: Vec<u32>,
    ) -> MergePlan {
        MergePlan {
            from: 0,
            to: 1,
            kind,
            absorbed: BTreeSet::from([0, 1]),
            partitions: vec![PartitionPlan {
                disk: 0,
                partition: 0,
                reserved,
                blocks,
                positions,
            }],
            out_file_number: 1,
            out_increment_number: 1,
            metadata_bytes: 0,
        }
    }

    fn write(plan: &MergePlan) -> (Vec<u8>, DataRegion, FakeSource) {
        let mut source = FakeSource::new();
        let mut out = Vec::new();
        let region = write_data_region(plan, &mut source, &mut out, 1).unwrap();
        (out, region, source)
    }

    #[test]
    fn copied_bytes_match_their_source_exactly() {
        let a = element(0, 1000, 64);
        let b = element(1, 2048, 128);
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![Action::Copy(a), Action::Copy(b)],
            Vec::new(),
        );
        let (out, region, _) = write(&plan);

        assert_eq!(region.payload_bytes, 192);
        assert_eq!(&out[0..64], &FakeSource::expected(0, 1000, 64)[..]);
        assert_eq!(&out[64..192], &FakeSource::expected(1, 2048, 128)[..]);
    }

    #[test]
    fn the_hash_and_the_length_are_never_altered() {
        let a = element(0, 1000, 64);
        let plan = plan_with(MergeKind::SyntheticFull, vec![Action::Copy(a)], Vec::new());
        let (_, region, _) = write(&plan);

        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(entries[0].md5_hash, a.md5_hash);
        assert_eq!(entries[0].block_length, a.block_length);
        // Only these two change.
        assert_eq!(entries[0].file_position, 0);
        assert_eq!(entries[0].file_number, 1);
    }

    #[test]
    fn a_hole_writes_nothing_and_stays_a_hole() {
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![Action::Hole, Action::Copy(element(0, 0, 32)), Action::Hole],
            Vec::new(),
        );
        let (out, region, source) = write(&plan);

        assert_eq!(out.len(), 4096, "32 bytes of payload, padded");
        assert_eq!(region.payload_bytes, 32);
        assert_eq!(source.reads.len(), 1, "a hole is never read");

        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(entries[0], DataBlockIndexElement::default());
        assert_eq!(entries[2], DataBlockIndexElement::default());
        assert_eq!(entries[1].file_position, 0);
    }

    #[test]
    fn a_kept_reference_is_written_through_untouched() {
        let kept = element(7, 9999, 64);
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![Action::Keep(kept)],
            Vec::new(),
        );
        let (out, region, source) = write(&plan);

        assert!(source.reads.is_empty(), "a kept block is never read");
        assert!(
            out.is_empty(),
            "a kept block writes nothing, and 0 needs no padding"
        );
        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(entries[0], kept, "file number and position both survive");
    }

    #[test]
    fn a_block_referenced_twice_is_copied_once() {
        let shared = element(0, 4096, 64);
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![
                Action::Copy(shared),
                Action::Copy(element(0, 8192, 64)),
                Action::Copy(shared),
            ],
            Vec::new(),
        );
        let (out, region, source) = write(&plan);

        assert_eq!(source.reads.len(), 2, "the repeat is served from the map");
        assert_eq!(region.duplicates_avoided, 1);
        assert_eq!(region.payload_bytes, 128, "two blocks, not three");
        assert_eq!(out.len(), 4096);

        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(
            entries[0].file_position, entries[2].file_position,
            "both entries point at the one copy"
        );
        assert_ne!(entries[0].file_position, entries[1].file_position);
    }

    #[test]
    fn two_blocks_that_differ_only_in_owner_are_both_copied() {
        // Same offset and length in two different files is not the same block.
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![
                Action::Copy(element(0, 512, 64)),
                Action::Copy(element(1, 512, 64)),
            ],
            Vec::new(),
        );
        let (_, region, source) = write(&plan);
        assert_eq!(source.reads.len(), 2);
        assert_eq!(region.duplicates_avoided, 0);
    }

    #[test]
    fn the_region_ends_on_an_aligned_boundary() {
        for (length, expected) in [(1u32, 4096u64), (4096, 4096), (4097, 8192), (8192, 8192)] {
            let plan = plan_with(
                MergeKind::SyntheticFull,
                vec![Action::Copy(element(0, 0, length))],
                Vec::new(),
            );
            let (out, region, _) = write(&plan);
            assert_eq!(region.end, expected, "payload of {length}");
            assert_eq!(out.len() as u64, expected);
            assert_eq!(region.end % 4096, 0);
            // The padding is zeros, not leftover data.
            assert!(out[length as usize..].iter().all(|b| *b == 0));
        }
    }

    #[test]
    fn reserved_sectors_are_copied_before_the_data_blocks_of_their_partition() {
        let plan = plan_with_reserved(
            MergeKind::SyntheticFull,
            vec![Action::Copy(element(0, 0, 16))],
            vec![Action::Copy(element(0, 64, 32))],
            Vec::new(),
        );
        let (_, region, source) = write(&plan);

        assert_eq!(source.reads[0], (0, 0, 16), "reserved first");
        assert_eq!(source.reads[1], (0, 64, 32));
        assert_eq!(region.partitions[0].index.reserved.len(), 1);
        assert_eq!(region.partitions[0].index.reserved[0].file_position, 0);
        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(entries[0].file_position, 16);
    }

    #[test]
    fn an_incremental_merge_produces_a_delta_index() {
        let plan = plan_with(
            MergeKind::IncrementalMerge,
            vec![
                Action::Copy(element(1, 0, 64)),
                Action::Copy(element(1, 64, 64)),
            ],
            vec![9, 40],
        );
        let (_, region, _) = write(&plan);

        let Blocks::Delta(deltas) = &region.partitions[0].index.blocks else {
            panic!("expected a delta index");
        };
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].block_index, 9);
        assert_eq!(deltas[1].block_index, 40);
        assert_eq!(deltas[0].element.file_position, 0);
        assert_eq!(deltas[1].element.file_position, 64);
    }

    #[test]
    fn a_delta_plan_without_matching_positions_is_rejected() {
        let plan = plan_with(
            MergeKind::IncrementalMerge,
            vec![Action::Copy(element(1, 0, 64))],
            vec![9, 40],
        );
        let mut source = FakeSource::new();
        let mut out = Vec::new();
        let err = write_data_region(&plan, &mut source, &mut out, 1).unwrap_err();
        assert!(format!("{err:#}").contains("positions"), "{err:#}");
    }

    #[test]
    fn the_index_serializes_to_the_expected_length() {
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![Action::Copy(element(0, 0, 64)), Action::Hole],
            Vec::new(),
        );
        let (_, region, _) = write(&plan);
        // Two counts of 4 bytes, no reserved entries, two 30-byte elements.
        assert_eq!(region.partitions[0].index.to_bytes().len(), 4 + 4 + 60);
    }

    /// Where the fixture's data region ends and its metadata region starts.
    const DATA_END: u64 = 4096;

    /// A source over one whole file image, so a verbatim copy can be compared against the
    /// bytes it came from.
    struct ImageSource {
        image: Vec<u8>,
    }

    impl BlockSource for ImageSource {
        fn read_block(&mut self, _file_number: u16, position: i64, length: u32) -> Result<Vec<u8>> {
            let at = position as usize;
            Ok(self.image[at..at + length as usize].to_vec())
        }
    }

    fn push_block(image: &mut Vec<u8>, name: &[u8; block::NAME_LEN], payload: &[u8], last: bool) {
        let header = MetadataBlockHeader::for_payload(name, payload, last).unwrap();
        image.extend_from_slice(&header.to_bytes());
        image.extend_from_slice(payload);
    }

    /// A To file of one disk and one partition, with an undocumented block in the partition
    /// list. Returns the source, its disks, and the offsets of the disk list and of the
    /// `$INDEX` that ends the partition list.
    fn to_file() -> (ImageSource, Vec<Disk>, u64, u64) {
        let mut image = vec![0x5au8; DATA_END as usize];
        let disk_at = image.len() as u64;
        push_block(&mut image, block::TRACK0, &[1u8; 512], false);
        push_block(&mut image, block::EPT, &[2u8; 96], true);
        let partition_at = image.len() as u64;
        push_block(&mut image, block::BITMAP, &[3u8; 300], false);
        push_block(&mut image, b"$WEIRD  ", &[4u8; 17], false);
        let index_at = image.len() as u64;
        // Two zero counts: an index of no reserved sectors and no blocks.
        push_block(&mut image, block::INDEX, &[0u8; 8], true);

        let mut cursor = Cursor::new(image.clone());
        let disk_blocks = block::walk_list(&mut cursor, disk_at).unwrap();
        let partition_blocks = block::walk_list(&mut cursor, partition_at).unwrap();
        let disks = vec![Disk {
            blocks: disk_blocks,
            partitions: vec![Partition {
                blocks: partition_blocks,
                index: PartitionIndex {
                    reserved: Vec::new(),
                    blocks: Blocks::Full(Vec::new()),
                },
            }],
        }];
        (ImageSource { image }, disks, disk_at, index_at)
    }

    /// The index the merge produced, which replaces the one the To file holds.
    fn written() -> DataRegion {
        DataRegion {
            partitions: vec![WrittenPartition {
                disk: 0,
                partition: 0,
                index: PartitionIndex {
                    reserved: vec![element(1, 0, 64)],
                    blocks: Blocks::Full(vec![
                        element(1, 64, 64),
                        DataBlockIndexElement::default(),
                    ]),
                },
            }],
            payload_bytes: 128,
            end: DATA_END,
            duplicates_avoided: 0,
        }
    }

    /// Write the metadata region of the fixture and return the output file it produces,
    /// data region included, together with the offset the writer reports.
    fn write_metadata(
        source: &mut ImageSource,
        disks: &[Disk],
        data: &DataRegion,
    ) -> (Vec<u8>, u64) {
        let mut out = Vec::new();
        let end = write_metadata_region(disks, data, source, &mut out, 1).unwrap();
        let mut file = vec![0u8; data.end as usize];
        file.extend_from_slice(&out);
        (file, end)
    }

    #[test]
    fn every_block_before_the_index_is_copied_byte_for_byte() {
        let (mut source, disks, disk_at, index_at) = to_file();
        let (file, _) = write_metadata(&mut source, &disks, &written());

        let span = disk_at as usize..index_at as usize;
        assert_eq!(file[span.clone()], source.image[span]);
    }

    #[test]
    fn the_fresh_index_ends_the_partition_list_and_is_stored_plain() {
        let (mut source, disks, disk_at, _) = to_file();
        let (file, end) = write_metadata(&mut source, &disks, &written());
        assert_eq!(end, file.len() as u64);

        let mut cursor = Cursor::new(file);
        let disk_list = block::walk_list(&mut cursor, disk_at).unwrap();
        let partition_list = block::walk_list(&mut cursor, disk_list.end).unwrap();

        let names: Vec<String> = partition_list
            .blocks
            .iter()
            .map(|b| b.header.name_str())
            .collect();
        assert_eq!(names, ["$BITMAP", "$WEIRD", "$INDEX"]);
        assert_eq!(partition_list.end, end);

        let index = partition_list.last();
        assert!(index.header.flags.last_block);
        assert!(!index.header.flags.compression);
        assert!(!index.header.flags.encryption);
    }

    #[test]
    fn the_index_hash_covers_the_stored_bytes() {
        let (mut source, disks, disk_at, _) = to_file();
        let data = written();
        let (file, _) = write_metadata(&mut source, &disks, &data);

        let mut cursor = Cursor::new(file);
        let disk_list = block::walk_list(&mut cursor, disk_at).unwrap();
        let partition_list = block::walk_list(&mut cursor, disk_list.end).unwrap();
        // read_block checks the hash against the stored bytes before it returns them.
        let payload = block::read_block(&mut cursor, partition_list.last()).unwrap();

        assert_eq!(payload, data.partitions[0].index.to_bytes());
    }

    #[test]
    fn a_data_region_that_does_not_match_the_to_file_is_refused() {
        let (mut source, disks, _, _) = to_file();
        let mut data = written();
        data.partitions.clear();

        let err =
            write_metadata_region(&disks, &data, &mut source, &mut Vec::new(), 1).unwrap_err();

        assert!(err.to_string().contains("describes 0 partitions"));
    }

    /// A document shaped like a real one: paths from the machine that took the backup, no
    /// `merged_files` key, and a file history that names one file outside the merge.
    fn document() -> Value {
        serde_json::json!({
            "_auxiliary_data": {
                "backup_definition": {
                    "backup_definition_file": "C:\\Users\\someone\\Documents\\Reflect\\Full.xml",
                    "consolidation_type": "none",
                    "filename": "C:\\Users\\someone\\Desktop\\SET-01-01.mrimgx"
                },
                "destination": "C:\\Users\\someone\\Desktop\\SET-01-01.mrimgx"
            },
            "_header": {
                "backup_type": "inc",
                "delta_index": true,
                "file_number": 1,
                "imageid": "DD5A77E6B68A6C34",
                "increment_number": 1,
                "index_file_position": 4096,
                "netbios_name": "DESKTOP-EXAMPLE",
                "split_file": false
            },
            "disks": [{
                "_geometry": { "disk_size": 534643200u64 },
                "partitions": [{
                    "_header": {
                        "file_history": [
                            {
                                "file_name": "C:\\Users\\someone\\Desktop\\SET-00-00.mrimgx",
                                "file_number": 0
                            },
                            {
                                "file_name": "C:\\Users\\someone\\Desktop\\SET-01-01.mrimgx",
                                "file_number": 1
                            },
                            {
                                "file_name": "D:\\elsewhere\\SET-07-07.mrimgx",
                                "file_number": 7
                            }
                        ],
                        "file_history_count": 3
                    }
                }]
            }]
        })
    }

    #[test]
    fn the_patched_header_records_what_the_output_is() {
        let plan = plan_with(MergeKind::SyntheticFull, Vec::new(), Vec::new());
        let mut doc = document();

        patch_header(&mut doc, &plan, 8192).unwrap();

        let header = &doc["_header"];
        assert_eq!(header["file_number"], 1);
        assert_eq!(header["increment_number"], 1);
        // The key was absent, so it is inserted. File 1 is the output itself, not something
        // it absorbed.
        assert_eq!(header["merged_files"], serde_json::json!([0]));
        assert_eq!(header["index_file_position"], 8192);
        assert_eq!(header["delta_index"], false);
        assert_eq!(header["split_file"], false);
        assert_eq!(header["backup_type"], "full");
        // The machine that took the backup is not the machine that wrote this file.
        assert_eq!(header["netbios_name"], "");
    }

    #[test]
    fn an_incremental_merge_keeps_the_delta_index_and_the_backup_type() {
        let plan = plan_with(MergeKind::IncrementalMerge, Vec::new(), Vec::new());
        let mut doc = document();

        patch_header(&mut doc, &plan, 8192).unwrap();

        assert_eq!(doc["_header"]["delta_index"], true);
        assert_eq!(doc["_header"]["backup_type"], "inc");
    }

    #[test]
    fn the_file_history_names_files_and_carries_no_path() {
        let plan = plan_with(MergeKind::SyntheticFull, Vec::new(), Vec::new());
        let mut doc = document();

        patch_partitions(&mut doc, &plan, "SET-01-01.mrimgx").unwrap();

        let header = &doc["disks"][0]["partitions"][0]["_header"];
        let history = header["file_history"].as_array().unwrap();
        // Files 0 and 1 are absorbed, so their bytes live in the output. File 7 is not, so
        // it keeps its own name with the directory removed.
        assert_eq!(history[0]["file_name"], "SET-01-01.mrimgx");
        assert_eq!(history[1]["file_name"], "SET-01-01.mrimgx");
        assert_eq!(history[2]["file_name"], "SET-07-07.mrimgx");
        assert_eq!(header["file_history_count"], 3);
    }

    #[test]
    fn the_auxiliary_data_records_the_merge_and_drops_the_paths() {
        let plan = plan_with(MergeKind::IncrementalMerge, Vec::new(), Vec::new());
        let mut doc = document();

        patch_auxiliary_data(&mut doc, &plan, "OUT.mrimgx").unwrap();

        let aux = &doc["_auxiliary_data"];
        assert_eq!(
            aux["backup_definition"]["consolidation_type"],
            "incremental_merge"
        );
        assert_eq!(aux["backup_definition"]["filename"], "OUT.mrimgx");
        assert_eq!(
            aux["backup_definition"]["backup_definition_file"],
            "Full.xml"
        );
        assert_eq!(aux["destination"], "OUT.mrimgx");
    }

    #[test]
    fn a_synthetic_full_takes_disk_size_from_the_file_with_the_full_index() {
        let mut doc = document();
        // The Full records the true device size. The To file rounded it down to a cylinder.
        let base = serde_json::json!({ "disks": [{ "_geometry": { "disk_size": 534672384u64 } }] });

        patch_disk_sizes(&mut doc, &base).unwrap();

        assert_eq!(doc["disks"][0]["_geometry"]["disk_size"], 534672384u64);
    }

    #[test]
    fn a_disk_count_that_does_not_match_the_full_index_file_is_refused() {
        let mut doc = document();
        let base = serde_json::json!({
            "disks": [
                { "_geometry": { "disk_size": 1u64 } },
                { "_geometry": { "disk_size": 2u64 } }
            ]
        });

        let err = patch_disk_sizes(&mut doc, &base).unwrap_err();

        assert!(err.to_string().contains("1 disks"), "{err}");
    }
}
