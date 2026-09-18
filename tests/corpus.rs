//! Tests that run against real backup files.
//!
//! The corpus lives in the gitignored `testdata/` directory, fetched from the
//! `linux-contrib-tidy` branch of `macrium/mrimgx_file_layout`. It is large and not
//! redistributable here, so every test skips when it is absent rather than failing. That
//! keeps a fresh clone green while still catching regressions on a working checkout.
//!
//! To fetch it:
//!
//! ```sh
//! git clone --filter=blob:none --no-checkout --branch linux-contrib-tidy \
//!     https://github.com/macrium/mrimgx_file_layout.git /tmp/mrimgx-corpus
//! cd /tmp/mrimgx-corpus
//! git sparse-checkout set --no-cone 'contrib/extract-to-img/Backup-Files/*'
//! git checkout
//! cp -r contrib/extract-to-img/Backup-Files/* <repo>/testdata/
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mrimgx_consolidate::block;
use mrimgx_consolidate::index::Blocks;
use mrimgx_consolidate::json;
use mrimgx_consolidate::plan;
use mrimgx_consolidate::reader::BackupFile;
use mrimgx_consolidate::scan;
use mrimgx_consolidate::set::BackupSet;
use mrimgx_consolidate::verify;
use mrimgx_consolidate::write;

fn corpus() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    dir.is_dir().then_some(dir)
}

/// Every `.mrimg` file anywhere under `testdata/`.
fn every_file(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            every_file(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e == "mrimg" || e == "mrimgx")
        {
            out.push(path);
        }
    }
}

fn all_files() -> Vec<PathBuf> {
    let Some(dir) = corpus() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    every_file(&dir, &mut out);
    out.sort();
    out
}

#[test]
fn every_corpus_file_parses_and_its_metadata_walk_lands_on_the_footer() {
    let files = all_files();
    if files.is_empty() {
        eprintln!("skipping: testdata/ is absent");
        return;
    }
    for path in &files {
        let file =
            BackupFile::open(path, true).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        // Any drift anywhere in the metadata region shows up here as a wrong tail length.
        file.check_framing()
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(file.trailing_bytes(), block::FOOTER_LEN);
    }
    eprintln!("checked {} files", files.len());
}

#[test]
fn every_corpus_json_payload_round_trips_byte_for_byte() {
    // This is the property that lets the writer patch metadata by parsing and re-emitting
    // rather than by editing raw bytes. If it ever stops holding, the writer must refuse.
    let files = all_files();
    if files.is_empty() {
        eprintln!("skipping: testdata/ is absent");
        return;
    }
    for path in &files {
        let file = BackupFile::open(path, false).unwrap();
        json::check_round_trip(&file.json, &file.json_raw)
            .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
    }
}

#[test]
fn the_metadata_region_starts_on_the_next_aligned_boundary_after_the_data() {
    // The writer must reproduce this exactly. The uncompressed corpus hides it, because
    // uncompressed blocks end on the boundary anyway and the gap reads as zero. A real
    // compressed set shows gaps of one to four kilobytes.
    let files = all_files();
    if files.is_empty() {
        eprintln!("skipping: testdata/ is absent");
        return;
    }
    for path in &files {
        let file = BackupFile::open(path, true).unwrap();
        let data_end = file.own_data_end();
        let ifp = file.header.index_file_position;
        assert_eq!(
            ifp % block::DATA_ALIGNMENT,
            0,
            "{}: index_file_position {ifp} is not a multiple of {}",
            path.display(),
            block::DATA_ALIGNMENT
        );
        assert_eq!(
            ifp,
            block::align_up(data_end),
            "{}: data ends at {data_end}, so the metadata region should start at {} \
             but starts at {ifp}",
            path.display(),
            block::align_up(data_end)
        );
    }
}

#[test]
fn no_corpus_index_block_is_compressed_or_encrypted() {
    // The reference reader re-reads the $INDEX payload raw after a rewind, which only
    // works when it is stored plain. BackupFile::open already refuses anything else, so
    // this test states the invariant rather than discovering it.
    let files = all_files();
    if files.is_empty() {
        eprintln!("skipping: testdata/ is absent");
        return;
    }
    for path in &files {
        let file = BackupFile::open(path, true).unwrap();
        for disk in &file.disks {
            for part in &disk.partitions {
                let flags = part.blocks.last().header.flags;
                assert!(
                    !flags.compression && !flags.encryption,
                    "{}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn flattening_the_two_file_set_resolves_every_member() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set/DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !target.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    assert_eq!(set.members.len(), 2);
    // Newest first.
    assert_eq!(set.members[0].header.file_number, 1);
    assert_eq!(set.members[1].header.file_number, 0);

    let flat = set.flatten().unwrap();
    assert_eq!(flat.disks.len(), 1);
    assert_eq!(flat.disks[0].len(), 1);

    // Every live block must name a file the set can actually open.
    for e in flat.blocks() {
        assert!(
            set.owner(e.file_number).is_some(),
            "block names file {} which no member owns",
            e.file_number
        );
    }

    let counts = flat.blocks_per_file();
    assert_eq!(counts[&0], 189);
    assert_eq!(counts[&1], 48);

    // The delta file holds 48 entries and all 48 survive, because nothing follows it.
    let Blocks::Delta(deltas) = &set.members[0].disks[0].partitions[0].index.blocks else {
        panic!("file 01-01 should carry a delta index");
    };
    assert_eq!(deltas.len(), 48);
}

#[test]
fn flattening_the_multi_partition_set_is_stable_at_every_resolution_point() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let dir = dir.join("Backup-Set-MP");
    if !dir.exists() {
        eprintln!("skipping: the multi-partition set is absent");
        return;
    }

    // Resolving as of each file in turn must never leave a block pointing at a file
    // outside the set, and must never lose a partition.
    let mut previous_live = 0;
    for increment in 0..=3 {
        let target = dir.join(format!(
            "584221F3840B0DBE-MP-Full-0{increment}-0{increment}.mrimg"
        ));
        let set = BackupSet::discover(&target).unwrap();
        assert_eq!(set.members.len(), increment + 1);

        let flat = set.flatten().unwrap();
        assert_eq!(flat.disks.len(), 1, "one disk");
        assert_eq!(flat.disks[0].len(), 3, "three partitions");

        for e in flat.blocks() {
            assert!(
                e.file_number as usize <= increment,
                "resolving as of file {increment} produced a block from file {}",
                e.file_number
            );
            assert!(set.owner(e.file_number).is_some());
        }

        // A later increment never captures fewer blocks: deltas replace or add, never
        // remove.
        let live = flat.blocks().count();
        assert!(
            live >= previous_live,
            "increment {increment} has {live} live blocks, down from {previous_live}"
        );
        previous_live = live;
    }
    assert_eq!(previous_live, 676);
}

#[test]
fn a_real_compressed_set_parses_and_resolves() {
    // The uncompressed corpus never sets the compression flag on $JSON, so a reader built
    // only against it refuses every real file. This set catches that.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("NOPASS/B5D6313DA329C717-NOPASS-02-02.mrimgx");
    if !target.exists() {
        eprintln!("skipping: the compressed set is absent");
        return;
    }

    let newest = BackupFile::open(&target, true).unwrap();
    assert!(
        newest
            .root_list
            .find(block::JSON)
            .unwrap()
            .header
            .flags
            .compression,
        "this set is supposed to have a compressed $JSON block"
    );

    let set = BackupSet::discover(&target).unwrap();
    assert_eq!(set.members.len(), 3);
    let flat = set.flatten().unwrap();
    for e in flat.blocks() {
        assert!(set.owner(e.file_number).is_some());
    }
    assert_eq!(flat.blocks().count(), 55806);
}

#[test]
fn an_encrypted_set_parses_and_resolves_without_a_password() {
    // This is the claim the whole design rests on. Only data blocks are encrypted, so
    // every offset a consolidation needs is readable without a key, and the merge itself
    // never has to decrypt anything.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("PASS/E9A7F5B2D6166D7C-NOPASS-02-02.mrimgx");
    if !target.exists() {
        eprintln!("skipping: the encrypted set is absent");
        return;
    }

    let newest = BackupFile::open(&target, true).unwrap();
    assert_eq!(
        newest.json["_encryption"]["enable"],
        serde_json::Value::Bool(true),
        "this set is supposed to be encrypted"
    );
    // $JSON is never encrypted: the reader must parse it before it can derive a key.
    assert!(
        !newest
            .root_list
            .find(block::JSON)
            .unwrap()
            .header
            .flags
            .encryption
    );

    let set = BackupSet::discover(&target).unwrap();
    assert_eq!(set.members.len(), 3);
    let flat = set.flatten().unwrap();
    assert_eq!(flat.blocks().count(), 62127);

    // Every member contributes, which is what makes this set worth keeping: the
    // uncompressed corpus and the NOPASS set both have near-empty increments.
    let counts = flat.blocks_per_file();
    assert_eq!(counts[&0], 53503);
    assert_eq!(counts[&1], 4148);
    assert_eq!(counts[&2], 4476);
}

#[test]
fn no_metadata_block_this_crate_must_read_is_ever_encrypted() {
    // $BITMAP is the only block that carries the encryption flag anywhere in the corpus,
    // and it is empty, because a bitmap exists only for exFAT and ReFS. A non-empty
    // encrypted $BITMAP is still untested. The writer copies it verbatim and so does not
    // care, but inspect would refuse it.
    let files = all_files();
    if files.is_empty() {
        eprintln!("skipping: testdata/ is absent");
        return;
    }
    for path in &files {
        let file = BackupFile::open(path, true).unwrap();
        for located in &file.root_list.blocks {
            if located.header.flags.encryption {
                assert_eq!(
                    located.header.block_length,
                    0,
                    "{}: a non-empty encrypted {} block in the root list",
                    path.display(),
                    located.header.name_str()
                );
            }
        }
        for disk in &file.disks {
            let lists =
                std::iter::once(&disk.blocks).chain(disk.partitions.iter().map(|p| &p.blocks));
            for list in lists {
                for located in &list.blocks {
                    if located.header.flags.encryption {
                        assert_eq!(
                            located.header.block_length,
                            0,
                            "{}: a non-empty encrypted {} block",
                            path.display(),
                            located.header.name_str()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn every_file_owns_its_whole_reserved_sector_array() {
    // buildIndex merges only data_blocks, and the restore takes the reserved array
    // wholesale from the newest file. That only works because each file re-stores its own
    // reserved sectors in full. The writer copies this array rather than flattening it.
    let files = all_files();
    if files.is_empty() {
        eprintln!("skipping: testdata/ is absent");
        return;
    }
    for path in &files {
        let file = BackupFile::open(path, true).unwrap();
        for disk in &file.disks {
            for part in &disk.partitions {
                for e in part.index.reserved.iter().filter(|e| !e.is_hole()) {
                    assert_eq!(
                        e.file_number,
                        file.header.file_number,
                        "{}: a reserved sector block names file {} rather than its own",
                        path.display(),
                        e.file_number
                    );
                }
            }
        }
    }
}

#[test]
fn disk_size_is_not_stable_across_a_set() {
    // A trap, recorded as a test so that it is not rediscovered the hard way.
    //
    // A Full records the true device size. An Incremental of the same set records the
    // size derived from the CHS geometry, which is smaller because it rounds down to a
    // whole cylinder. Every file of a set carries identical cylinder, head and sector
    // fields, so only disk_size disagrees.
    //
    // A synthetic Full must therefore take disk_size from the file carrying the full
    // index, never from the To file. Otherwise the output claims a smaller disk than the
    // original, and a restore of the output truncates the tail of the device.
    let files = all_files();
    if files.is_empty() {
        eprintln!("skipping: testdata/ is absent");
        return;
    }

    let mut fulls = 0;
    let mut increments = 0;
    for path in &files {
        let file = BackupFile::open(path, false).unwrap();
        let g = &file.json["disks"][0]["_geometry"];
        let field = |k: &str| g[k].as_u64().unwrap_or(0);
        let chs = field("cylinders")
            * field("sectors_per_track")
            * field("tracks_per_cylinder")
            * field("bytes_per_sector");
        if chs == 0 {
            continue;
        }
        let disk_size = field("disk_size");
        if file.header.is_full_index() {
            // The true device size is at or above the cylinder-aligned size.
            assert!(
                disk_size >= chs,
                "{}: a Full reports disk_size {disk_size} below its CHS size {chs}",
                path.display()
            );
            fulls += 1;
        } else {
            assert_eq!(
                disk_size,
                chs,
                "{}: an Incremental should report the CHS-derived size",
                path.display()
            );
            increments += 1;
        }
    }
    assert!(
        fulls > 0 && increments > 0,
        "the corpus must cover both cases"
    );
    eprintln!("checked {fulls} full-index files and {increments} increments");
}

#[test]
fn a_set_resolved_as_of_the_full_holds_only_the_full() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set/DD5A77E6B68A6C34-Full-00-00.mrimg");
    if !target.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }
    // increment_number <= target filters the later file out, even though it sits in the
    // same directory.
    let set = BackupSet::discover(&target).unwrap();
    assert_eq!(set.members.len(), 1);
    let flat = set.flatten().unwrap();
    assert_eq!(
        flat.blocks_per_file().keys().copied().collect::<Vec<_>>(),
        vec![0]
    );
}

#[test]
fn planning_a_synthetic_full_moves_every_live_block() {
    // From is the Full, so every block resolves into the merge and nothing keeps an old
    // reference. The counts come from `resolve` on the same set.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set/DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !target.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    let plan = plan::build(&set, 0, 1).unwrap();

    assert_eq!(plan.kind, plan::MergeKind::SyntheticFull);
    assert!(
        !plan.kind.delta_index(),
        "a synthetic Full carries a full index"
    );
    assert_eq!(plan.kind.consolidation_type(), "synthetic_full");
    assert_eq!(plan.redundant_file_numbers(), vec![0, 1]);
    assert_eq!(plan.blocks_to_copy(), 237);
    assert_eq!(plan.blocks_kept(), 0, "nothing survives a full merge");
    assert_eq!(plan.bytes_to_copy(), 15532032);
    assert!(plan.projected_size() > plan.bytes_to_copy());
}

#[test]
fn planning_a_middle_range_keeps_the_blocks_it_does_not_absorb() {
    // The four-file set resolved as of file 3 draws 561, 43, 27 and 45 blocks from its
    // four members. Merging from file 1 leaves the Full's 561 blocks alone.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set-MP/584221F3840B0DBE-MP-Full-03-03.mrimg");
    if !target.exists() {
        eprintln!("skipping: the multi-partition set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();

    let full = plan::build(&set, 0, 3).unwrap();
    assert_eq!(full.kind, plan::MergeKind::SyntheticFull);
    assert_eq!(full.blocks_to_copy(), 676);
    assert_eq!(full.blocks_kept(), 0);

    let from_one = plan::build(&set, 1, 3).unwrap();
    assert_eq!(from_one.kind, plan::MergeKind::IncrementalMerge);
    assert!(from_one.kind.delta_index());
    assert_eq!(from_one.redundant_file_numbers(), vec![1, 2, 3]);
    assert_eq!(from_one.blocks_to_copy(), 43 + 27 + 45);

    let from_two = plan::build(&set, 2, 3).unwrap();
    assert_eq!(from_two.blocks_to_copy(), 27 + 45);
    assert_eq!(from_two.redundant_file_numbers(), vec![2, 3]);

    // Merging from later in the chain always moves less.
    assert!(full.bytes_to_copy() > from_one.bytes_to_copy());
    assert!(from_one.bytes_to_copy() > from_two.bytes_to_copy());
}

#[test]
fn planning_an_incremental_merge_names_only_the_changed_positions() {
    // The output of an incremental merge holds a delta index. It must name every position
    // the absorbed members changed, and no more.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set-MP/584221F3840B0DBE-MP-Full-03-03.mrimg");
    if !target.exists() {
        eprintln!("skipping: the multi-partition set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    let plan = plan::build(&set, 2, 3).unwrap();

    for part in &plan.partitions {
        assert_eq!(
            part.positions.len(),
            part.blocks.len(),
            "every entry of a delta index needs its logical position"
        );
        let mut sorted = part.positions.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, part.positions, "positions ascend and never repeat");
    }
}

#[test]
fn planning_a_compressed_and_an_encrypted_set_reports_real_totals() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    for (name, expected) in [
        ("NOPASS/B5D6313DA329C717-NOPASS-02-02.mrimgx", 55806usize),
        ("PASS/E9A7F5B2D6166D7C-NOPASS-02-02.mrimgx", 62127),
    ] {
        let target = dir.join(name);
        if !target.exists() {
            eprintln!("skipping: {name} is absent");
            continue;
        }
        let set = BackupSet::discover(&target).unwrap();
        let plan = plan::build(&set, 0, 2).unwrap();
        assert_eq!(plan.kind, plan::MergeKind::SyntheticFull);
        // Reserved sector blocks move too, on top of the live data blocks.
        assert!(
            plan.blocks_to_copy() >= expected,
            "{name}: expected at least {expected} blocks, planned {}",
            plan.blocks_to_copy()
        );
        assert_eq!(plan.blocks_kept(), 0);
    }
}

#[test]
fn the_refusal_rules_use_the_documented_wording() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set/DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !target.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }
    let set = BackupSet::discover(&target).unwrap();

    // Reversed order.
    let err = plan::build(&set, 1, 0).unwrap_err();
    assert!(
        format!("{err:#}").contains("From file is more recent than the To file"),
        "{err:#}"
    );

    // The same file twice.
    let err = plan::build(&set, 1, 1).unwrap_err();
    assert!(format!("{err:#}").contains("nothing to merge"), "{err:#}");

    // A file number that is not in the set.
    assert!(plan::build(&set, 0, 9).is_err());
}

/// Read a byte range out of a file. Used to prove a copy against its source.
fn read_range(path: &Path, at: u64, len: u32) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).unwrap();
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf).unwrap();
    buf
}

#[test]
fn copying_the_data_region_reproduces_every_source_block_exactly() {
    // The claim the whole design rests on: a block moves between files unchanged. This
    // reads each written block back out of the output and compares it against the bytes
    // still sitting in the source file.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set/DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !target.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    let plan = plan::build(&set, 0, 1).unwrap();

    // Keep the source entries so each copy can be traced back to where it came from.
    let sources: Vec<plan::Action> = plan
        .partitions
        .iter()
        .flat_map(|p| p.reserved.iter().chain(p.blocks.iter()).copied())
        .collect();

    let out_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    {
        let mut source = write::SourceFiles::open(&set, &plan).unwrap();
        let file = std::fs::File::create(&out_path).unwrap();
        let mut out = std::io::BufWriter::new(file);
        let region = write::write_data_region(&plan, &mut source, &mut out, 1).unwrap();
        out.into_inner().unwrap().sync_all().unwrap();

        assert_eq!(region.payload_bytes, plan.bytes_to_copy());
        assert_eq!(region.end, block::align_up(region.payload_bytes));
        assert_eq!(region.end % block::DATA_ALIGNMENT, 0);
        assert_eq!(region.partitions.len(), 1);

        // Every written block must equal its source range, byte for byte.
        let Blocks::Full(written) = &region.partitions[0].index.blocks else {
            panic!("a synthetic Full carries a full index");
        };
        assert_eq!(written.len(), sources.len());

        let mut compared = 0;
        for (action, entry) in sources.iter().zip(written.iter()) {
            match action {
                plan::Action::Hole => {
                    assert_eq!(*entry, Default::default(), "a hole stays a hole");
                }
                plan::Action::Keep(original) => {
                    assert_eq!(entry, original, "a kept entry is untouched");
                }
                plan::Action::Copy(original) => {
                    assert_eq!(entry.block_length, original.block_length);
                    assert_eq!(entry.md5_hash, original.md5_hash);
                    assert_eq!(entry.file_number, 1, "copied blocks belong to the output");

                    let owner = set.owner(original.file_number).unwrap();
                    let expected = read_range(
                        &owner.path,
                        original.file_position as u64,
                        original.block_length,
                    );
                    let actual =
                        read_range(&out_path, entry.file_position as u64, entry.block_length);
                    assert_eq!(
                        actual,
                        expected,
                        "block copied from {}",
                        owner.path.display()
                    );
                    compared += 1;
                }
            }
        }
        assert_eq!(compared, 237, "every live block of this set was compared");
    }

    let written_len = std::fs::metadata(&out_path).unwrap().len();
    assert_eq!(written_len % block::DATA_ALIGNMENT, 0);
    assert_eq!(written_len, block::align_up(plan.bytes_to_copy()));
}

#[test]
fn copying_a_compressed_set_moves_the_stored_bytes_untouched() {
    // A compressed set is the case that would expose any accidental re-encoding, because
    // the stored bytes are a zstd frame rather than plain data.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("NOPASS/B5D6313DA329C717-NOPASS-02-02.mrimgx");
    if !target.exists() {
        eprintln!("skipping: the compressed set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    // Merge only the two small increments, so the test does not copy 3.7 GB.
    let plan = plan::build(&set, 1, 2).unwrap();
    assert_eq!(plan.kind, plan::MergeKind::IncrementalMerge);

    let out_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let mut source = write::SourceFiles::open(&set, &plan).unwrap();
    let file = std::fs::File::create(&out_path).unwrap();
    let mut out = std::io::BufWriter::new(file);
    let region = write::write_data_region(&plan, &mut source, &mut out, 2).unwrap();
    out.into_inner().unwrap().sync_all().unwrap();

    let Blocks::Delta(deltas) = &region.partitions[0].index.blocks else {
        panic!("an incremental merge carries a delta index");
    };
    assert!(!deltas.is_empty());

    // Spot-check every copied delta block against its source.
    for (action, delta) in plan.partitions[0].blocks.iter().zip(deltas.iter()) {
        let plan::Action::Copy(original) = action else {
            continue;
        };
        let owner = set.owner(original.file_number).unwrap();
        let expected = read_range(
            &owner.path,
            original.file_position as u64,
            original.block_length,
        );
        let actual = read_range(
            &out_path,
            delta.element.file_position as u64,
            delta.element.block_length,
        );
        assert_eq!(actual, expected);
    }

    // The reserved sectors are large and compressed. They must move too.
    assert_eq!(region.partitions[0].index.reserved.len(), 4);
    for entry in &region.partitions[0].index.reserved {
        assert_eq!(entry.file_number, 2);
        assert!(entry.block_length > 0);
    }
    assert_eq!(region.end % block::DATA_ALIGNMENT, 0);
}

/// Every string in `value` that starts with a drive letter, which is what a path from the
/// machine that took the backup looks like.
fn drive_letter_paths(value: &serde_json::Value, at: String, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                drive_letter_paths(child, format!("{at}.{key}"), found);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                drive_letter_paths(child, format!("{at}[{i}]"), found);
            }
        }
        serde_json::Value::String(text) => {
            let bytes = text.as_bytes();
            if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &text[1..3] == ":\\" {
                found.push(format!("{at} = {text}"));
            }
        }
        _ => {}
    }
}

#[test]
fn patching_a_real_document_records_the_merge_and_leaks_no_path() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set/DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !target.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    let plan = plan::build(&set, 0, 1).unwrap();
    let to = set.owner(1).unwrap();
    let before: serde_json::Value = serde_json::from_slice(&to.json_raw).unwrap();
    assert!(
        before["_header"].get("merged_files").is_none(),
        "an unconsolidated file has no merged_files key, so the patcher must insert it"
    );

    let patched = write::patch_document(&set, &plan, 16_777_216, "MERGED-00-00.mrimg").unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&patched).unwrap();

    assert_eq!(doc["_header"]["merged_files"], serde_json::json!([0]));
    assert_eq!(doc["_header"]["index_file_position"], 16_777_216u64);
    assert_eq!(doc["_header"]["delta_index"], false);
    assert_eq!(doc["_header"]["backup_type"], "full");
    assert_ne!(before["_header"]["netbios_name"], "");
    assert_eq!(doc["_header"]["netbios_name"], "");
    assert_eq!(
        doc["_auxiliary_data"]["backup_definition"]["consolidation_type"],
        "synthetic_full"
    );

    // The Full records the true device size. File 1 rounded it down to a whole cylinder.
    let full = set.base().unwrap();
    assert_eq!(full.header.file_number, 0);
    assert_eq!(
        doc["disks"][0]["_geometry"]["disk_size"],
        full.json["disks"][0]["_geometry"]["disk_size"]
    );
    assert_ne!(
        doc["disks"][0]["_geometry"]["disk_size"], before["disks"][0]["_geometry"]["disk_size"],
        "this set is the one that proves disk_size differs across a chain"
    );

    let mut leaks = Vec::new();
    drive_letter_paths(&doc, String::new(), &mut leaks);
    assert!(leaks.is_empty(), "paths survived the patch: {leaks:?}");
    // The instrument works: the source document carries five such paths.
    let mut before_leaks = Vec::new();
    drive_letter_paths(&before, String::new(), &mut before_leaks);
    assert!(!before_leaks.is_empty());

    // The patched document is still canonical, so it round-trips through the gate.
    let reparsed = json::parse(&patched).unwrap();
    json::check_round_trip(&reparsed, &patched).unwrap();
}

#[test]
fn the_written_output_reads_back_and_resolves_to_the_same_blocks() {
    // The whole write path end to end, checked with this crate's own reader. It proves the
    // output parses and resolves. It does not prove the image restores: only the extraction
    // comparison against the independent reference extractor proves that.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("Backup-Set/DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !target.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    let plan = plan::build(&set, 0, 1).unwrap();
    let before = set.flatten().unwrap();

    let out_dir = tempfile::tempdir().unwrap();
    let name = "DD5A77E6B68A6C34-Full-01-01.mrimg";
    let out_path = out_dir.path().join(name);
    let written = {
        let mut source = write::SourceFiles::open(&set, &plan).unwrap();
        let file = std::fs::File::create(&out_path).unwrap();
        let mut out = std::io::BufWriter::new(file);
        let written = write::write_output(&set, &plan, &mut source, &mut out, name).unwrap();
        out.into_inner().unwrap().sync_all().unwrap();
        written
    };

    assert_eq!(std::fs::metadata(&out_path).unwrap().len(), written.size);
    assert!(
        written.size <= plan.projected_size(),
        "the plan projected {} bytes and the write produced {}",
        plan.projected_size(),
        written.size
    );

    write::check_output(&out_path, &plan).unwrap();

    // The shipped command opens it, which is the check a user runs first.
    let inspect = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
        .arg("inspect")
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(
        inspect.status.success(),
        "inspect refused the output: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );

    // The metadata walk lands on the footer, which is what `inspect` reports.
    let output = BackupFile::open(&out_path, true).unwrap();
    output.check_framing().unwrap();
    assert_eq!(output.root_at, written.root_at);
    assert_eq!(output.trailing_bytes(), block::FOOTER_LEN);
    assert_eq!(output.header.index_file_position, written.data.end);
    assert_eq!(output.header.merged_files, vec![0]);
    assert!(
        !output.header.delta_index,
        "a synthetic Full is not a delta"
    );

    // The output claims both file numbers, so it is a complete set on its own.
    let after_set = BackupSet::discover(&out_path).unwrap();
    assert_eq!(after_set.members.len(), 1);
    let after = after_set.flatten().unwrap();

    assert_eq!(after.disks.len(), before.disks.len());
    let mut compared = 0;
    for (d, (new_disk, old_disk)) in after.disks.iter().zip(before.disks.iter()).enumerate() {
        assert_eq!(new_disk.len(), old_disk.len(), "disk {d}");
        for (p, (new_part, old_part)) in new_disk.iter().zip(old_disk.iter()).enumerate() {
            assert_eq!(new_part.len(), old_part.len(), "disk {d} partition {p}");
            for (i, (new, old)) in new_part.iter().zip(old_part.iter()).enumerate() {
                let at = format!("disk {d} partition {p} block {i}");
                assert_eq!(new.is_hole(), old.is_hole(), "{at}");
                if new.is_hole() {
                    continue;
                }
                assert_eq!(new.block_length, old.block_length, "{at}");
                assert_eq!(new.md5_hash, old.md5_hash, "{at}");
                assert_eq!(new.file_number, 1, "{at} belongs to the output");
                compared += 1;
            }
        }
    }
    assert_eq!(compared, plan.blocks_to_copy());
    assert_eq!(after.stored_bytes(), before.stored_bytes());
}

#[test]
fn an_incremental_merge_keeps_the_full_and_still_resolves() {
    // Files 1 through 3 of the four-file set merge into one Incremental. File 0 stays on
    // disk, so the output keeps references to it and carries a delta index.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let source_dir = dir.join("Backup-Set-MP");
    let full = source_dir.join("584221F3840B0DBE-MP-Full-00-00.mrimg");
    let target = source_dir.join("584221F3840B0DBE-MP-Full-03-03.mrimg");
    if !full.exists() || !target.exists() {
        eprintln!("skipping: the multi-partition set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    let plan = plan::build(&set, 1, 3).unwrap();
    assert_eq!(plan.kind, plan::MergeKind::IncrementalMerge);
    // The Full's blocks are not in this plan at all. A delta index names only the positions
    // the absorbed members changed, and the Full still supplies the rest at resolve time.
    assert_eq!(plan.blocks_to_copy(), 43 + 27 + 45);
    let before = set.flatten().unwrap();

    // The surviving Full has to sit beside the output, because a set is discovered by
    // scanning one directory.
    let out_dir = tempfile::tempdir().unwrap();
    std::fs::copy(&full, out_dir.path().join(full.file_name().unwrap())).unwrap();
    let name = "584221F3840B0DBE-MP-Full-03-03.mrimg";
    let out_path = out_dir.path().join(name);
    {
        let mut source = write::SourceFiles::open(&set, &plan).unwrap();
        let file = std::fs::File::create(&out_path).unwrap();
        let mut out = std::io::BufWriter::new(file);
        write::write_output(&set, &plan, &mut source, &mut out, name).unwrap();
        out.into_inner().unwrap().sync_all().unwrap();
    }

    write::check_output(&out_path, &plan).unwrap();
    let output = BackupFile::open(&out_path, true).unwrap();
    assert!(
        output.header.delta_index,
        "an incremental merge stays delta"
    );
    assert_eq!(output.header.merged_files, vec![1, 2]);

    let after = BackupSet::discover(&out_path).unwrap();
    assert_eq!(
        after.members.len(),
        2,
        "the Full and the merged Incremental"
    );
    let after = after.flatten().unwrap();

    assert_eq!(after.disks.len(), before.disks.len());
    for (d, (new_disk, old_disk)) in after.disks.iter().zip(before.disks.iter()).enumerate() {
        for (p, (new_part, old_part)) in new_disk.iter().zip(old_disk.iter()).enumerate() {
            assert_eq!(new_part.len(), old_part.len(), "disk {d} partition {p}");
            for (i, (new, old)) in new_part.iter().zip(old_part.iter()).enumerate() {
                let at = format!("disk {d} partition {p} block {i}");
                assert_eq!(new.is_hole(), old.is_hole(), "{at}");
                if new.is_hole() {
                    continue;
                }
                assert_eq!(new.block_length, old.block_length, "{at}");
                assert_eq!(new.md5_hash, old.md5_hash, "{at}");
                // A block either moved into the output or still belongs to the Full.
                assert!(
                    new.file_number == 3 || new.file_number == 0,
                    "{at} points at file {}",
                    new.file_number
                );
            }
        }
    }
    assert_eq!(after.stored_bytes(), before.stored_bytes());
    // Both files still supply blocks, and the absorbed numbers are gone.
    let per_file = after.blocks_per_file();
    assert_eq!(per_file.len(), 2);
    assert_eq!(per_file[&0], 561);
    assert_eq!(per_file[&3], 43 + 27 + 45);
}

#[test]
fn the_command_writes_a_merge_and_never_touches_a_source() {
    // The commit sequence through the shipped command: lock, temporary file, rename, then
    // the read-back. Every source must come out of it byte for byte as it went in.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let source_dir = dir.join("Backup-Set");
    let from = source_dir.join("DD5A77E6B68A6C34-Full-00-00.mrimg");
    let to = source_dir.join("DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !from.exists() || !to.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }

    let before: Vec<Vec<u8>> = [&from, &to]
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();

    let out_dir = tempfile::tempdir().unwrap();
    let out_path = out_dir.path().join("MERGED-00-00.mrimg");
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
        .args(["consolidate", "--from"])
        .arg(&from)
        .arg("--to")
        .arg(&to)
        .arg("--out")
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "the merge failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    assert!(out_path.is_file(), "the output is in place");
    let report = String::from_utf8_lossy(&run.stdout);
    assert!(
        report.contains("read back and checked against the plan"),
        "{report}"
    );
    assert!(report.contains("nothing was deleted"), "{report}");

    // The lock is released on the way out, so a second merge can run.
    assert!(!out_dir.path().join("merge_running").exists());

    let after: Vec<Vec<u8>> = [&from, &to]
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    assert_eq!(before, after, "a source file changed during the merge");

    // A lock left by another run stops the next one, and says what holds it.
    std::fs::write(
        out_dir.path().join("merge_running"),
        "pid 1\nstarted 0\nmerging files 0 through 1\n",
    )
    .unwrap();
    let blocked_path = out_dir.path().join("SECOND-00-00.mrimg");
    let blocked = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
        .args(["consolidate", "--from"])
        .arg(&from)
        .arg("--to")
        .arg(&to)
        .arg("--out")
        .arg(&blocked_path)
        .output()
        .unwrap();
    assert!(!blocked.status.success(), "a held lock must stop the run");
    let complaint = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        complaint.contains("another merge holds the lock"),
        "{complaint}"
    );
    assert!(
        complaint.contains("merging files 0 through 1"),
        "{complaint}"
    );
    assert!(!blocked_path.exists(), "nothing is written while blocked");
}

#[test]
fn every_refusal_exits_non_zero_and_writes_nothing() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let source_dir = dir.join("Backup-Set");
    let from = source_dir.join("DD5A77E6B68A6C34-Full-00-00.mrimg");
    let to = source_dir.join("DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !from.exists() || !to.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }
    let out_dir = tempfile::tempdir().unwrap();
    let taken = out_dir.path().join("TAKEN-00-00.mrimg");
    std::fs::write(&taken, b"a file that is already there").unwrap();

    let refuse = |args: &[&Path]| -> String {
        let run = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
            .arg("consolidate")
            .arg("--from")
            .arg(args[0])
            .arg("--to")
            .arg(args[1])
            .arg("--out")
            .arg(args[2])
            .output()
            .unwrap();
        assert!(!run.status.success(), "this run should have been refused");
        String::from_utf8_lossy(&run.stderr).to_string()
    };

    // The two files the wrong way round.
    let out = out_dir.path().join("MERGED-00-00.mrimg");
    let complaint = refuse(&[&to, &from, &out]);
    assert!(
        complaint.contains("From file is more recent than the To file"),
        "{complaint}"
    );
    assert!(!out.exists());

    // An output that names a file of the set.
    let complaint = refuse(&[&from, &to, &to]);
    assert!(
        complaint.contains("is a file of this backup set"),
        "{complaint}"
    );

    // An output that is already there.
    let complaint = refuse(&[&from, &to, &taken]);
    assert!(complaint.contains("already exists"), "{complaint}");
    assert_eq!(
        std::fs::read(&taken).unwrap(),
        b"a file that is already there",
        "the file that was there is untouched"
    );
}

/// The independent reference extractor, built by `scratch/build-refextract.sh`.
fn refextract() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var("REFEXTRACT").unwrap_or("/tmp/refextract".into()));
    path.is_file().then_some(path)
}

fn extract(oracle: &Path, backup: &Path, image: &Path) {
    let run = std::process::Command::new(oracle)
        .arg(backup)
        .arg(image)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{} refused {}: {}",
        oracle.display(),
        backup.display(),
        String::from_utf8_lossy(&run.stderr)
    );
}

/// The temporary output a run left in `directory`, if there is one.
fn leftover_temp(directory: &Path) -> Option<PathBuf> {
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("macrium_consolidation_temp-"))
        })
}

/// How many bytes a running process has written, from `/proc/<pid>/io`.
///
/// The length of the temporary file cannot be used: the run reserves its space up front, so
/// the file is full size from the start. This counts bytes handed to write calls instead.
fn bytes_written_by(pid: u32) -> u64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/io")) else {
        return 0;
    };
    text.lines()
        .find_map(|line| line.strip_prefix("wchar: "))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Start a merge, kill it once `fraction` of the output is written, and report how far it
/// got.
///
/// The kill is a SIGKILL, so nothing runs on the way out: no cleanup, no lock release. That
/// is the case this test exists for.
fn kill_part_way(from: &Path, to: &Path, out: &Path, fraction: f64, expected: u64) -> u64 {
    let stop_at = (expected as f64 * fraction) as u64;

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
        .arg("consolidate")
        .arg("--from")
        .arg(from)
        .arg("--to")
        .arg(to)
        .arg("--out")
        .arg(out)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let mut written;
    loop {
        written = bytes_written_by(child.id());
        if written >= stop_at {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the merge finished before it could be killed at {fraction}"
        );
    }

    child.kill().unwrap();
    child.wait().unwrap();
    written
}

/// Merge a range through the shipped command.
fn merge(from: &Path, to: &Path, out: &Path) {
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
        .arg("consolidate")
        .arg("--from")
        .arg(from)
        .arg("--to")
        .arg(to)
        .arg("--out")
        .arg(out)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "merging {} through {} failed: {}",
        from.display(),
        to.display(),
        String::from_utf8_lossy(&run.stderr)
    );
}

/// Compare two extractions, allowing for the `disk_size` difference between a Full and an
/// Incremental. The shorter image must match over its whole length, and the tail of the
/// longer one must be empty.
fn compare_images(chain: &Path, merged: &Path) {
    let chain_bytes = std::fs::read(chain).unwrap();
    let merge_bytes = std::fs::read(merged).unwrap();
    let common = chain_bytes.len().min(merge_bytes.len());

    assert_eq!(
        chain_bytes[..common],
        merge_bytes[..common],
        "the merged image differs from the chain image"
    );
    assert!(
        chain_bytes[common..].iter().all(|b| *b == 0)
            && merge_bytes[common..].iter().all(|b| *b == 0),
        "the tail past the end of the shorter image is not empty"
    );
}

#[test]
fn every_pair_of_the_multi_partition_set_merges_and_extracts() {
    // Six From and To pairs across a four-file, three-partition chain. Each merged file is
    // extracted by the oracle and compared against an extraction of the chain at the same
    // resolution point.
    //
    // The merged file is extracted directly rather than through a later member. The oracle
    // finds members by following file_history, and a later member's history still names the
    // files the merge absorbed. The merged file's own history names the merged file for
    // every number it absorbed, so the walk resolves. Our own reader finds members by
    // scanning the directory, as the reference reader does, and does not have this limit.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let Some(oracle) = refextract() else {
        eprintln!("skipping: build the oracle with scratch/build-refextract.sh");
        return;
    };
    let source_dir = dir.join("Backup-Set-MP");
    let name = |n: u16| format!("584221F3840B0DBE-MP-Full-{n:02}-{n:02}.mrimg");
    if !source_dir.join(name(3)).exists() {
        eprintln!("skipping: the multi-partition set is absent");
        return;
    }

    let work = tempfile::tempdir().unwrap();
    let mut baselines: HashMap<u16, PathBuf> = HashMap::new();

    for (from, to) in [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)] {
        let case = work.path().join(format!("merge-{from}-{to}"));
        std::fs::create_dir(&case).unwrap();
        let merged = case.join(format!("MERGED-{from:02}-{to:02}.mrimg"));
        merge(
            &source_dir.join(name(from)),
            &source_dir.join(name(to)),
            &merged,
        );

        let output = BackupFile::open(&merged, true).unwrap();
        output.check_framing().unwrap();
        assert_eq!(output.header.file_number, to);
        assert_eq!(
            output.header.merged_files,
            (from..to).map(i64::from).collect::<Vec<_>>(),
            "the output claims every number it absorbed"
        );
        // A merge that starts at the Full stands alone and carries a complete index. One
        // that starts at an Incremental still references the files below it.
        assert_eq!(
            output.header.delta_index,
            from != 0,
            "disagreement on the index form for {from} through {to}"
        );

        // The oracle needs the members the merge did not absorb beside the output.
        for number in 0..from {
            std::fs::copy(source_dir.join(name(number)), case.join(name(number))).unwrap();
        }

        let baseline = baselines.entry(to).or_insert_with(|| {
            let image = work.path().join(format!("chain-as-of-{to}.img"));
            extract(&oracle, &source_dir.join(name(to)), &image);
            image
        });
        let from_merge = case.join("merge.img");
        extract(&oracle, &merged, &from_merge);

        compare_images(baseline, &from_merge);
    }
}

#[test]
fn the_compressed_and_the_encrypted_sets_merge_whole() {
    // The sample corpus is uncompressed and unencrypted. These two sets are not, and a
    // merge must copy their stored bytes without decompressing or decrypting anything. The
    // encrypted set is merged with no password, because consolidation never needs one.
    //
    // Only the two increments of each set are merged. Merging from the Full would copy
    // 3.7 GB, which is what the ignored throughput test is for.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };

    for (label, target) in [
        (
            "high compression",
            "NOPASS/B5D6313DA329C717-NOPASS-02-02.mrimgx",
        ),
        ("AES-128", "PASS/E9A7F5B2D6166D7C-NOPASS-02-02.mrimgx"),
    ] {
        let target = dir.join(target);
        if !target.exists() {
            eprintln!("skipping: the {label} set is absent");
            continue;
        }

        let set = BackupSet::discover(&target).unwrap();
        let plan = plan::build(&set, 1, 2).unwrap();
        assert_eq!(plan.kind, plan::MergeKind::IncrementalMerge);
        assert!(plan.blocks_to_copy() > 0, "{label}: there is work to do");

        let out_dir = tempfile::tempdir().unwrap();
        let merged = out_dir.path().join("MERGED-02-02.mrimgx");
        let from = set.owner(1).unwrap().path.clone();
        merge(&from, &target, &merged);

        let output = BackupFile::open(&merged, true).unwrap();
        output.check_framing().unwrap();
        write::check_output(&merged, &plan).unwrap();
        assert_eq!(output.header.merged_files, vec![1], "{label}");

        // The settings are carried through, because the blocks were not re-encoded.
        let to = set.owner(2).unwrap();
        assert_eq!(
            output.json["_compression"], to.json["_compression"],
            "{label}"
        );
        assert_eq!(
            output.json["_encryption"], to.json["_encryption"],
            "{label}"
        );

        // Every copied block matches the bytes it came from, byte for byte.
        let mut compared = 0;
        for (part, written) in plan
            .partitions
            .iter()
            .zip(output.disks[0].partitions.iter())
        {
            let Blocks::Delta(deltas) = &written.index.blocks else {
                panic!("{label}: an incremental merge carries a delta index");
            };
            let pairs = part
                .blocks
                .iter()
                .zip(deltas.iter().map(|delta| &delta.element))
                // The reserved sectors hold the file allocation tables. They are large,
                // they are compressed, and every file re-stores them in full, so on the
                // high-compression set they are most of what moves.
                .chain(part.reserved.iter().zip(written.index.reserved.iter()));
            for (action, entry) in pairs {
                let plan::Action::Copy(original) = action else {
                    continue;
                };
                let owner = set.owner(original.file_number).unwrap();
                assert_eq!(
                    read_range(&merged, entry.file_position as u64, entry.block_length),
                    read_range(
                        &owner.path,
                        original.file_position as u64,
                        original.block_length
                    ),
                    "{label}: a copied block differs from its source"
                );
                compared += 1;
            }
        }
        assert_eq!(
            compared,
            plan.blocks_to_copy(),
            "{label}: every copied block is compared against its source"
        );
        eprintln!("{label}: {compared} copied blocks compared against their sources");
    }
}

/// Bytes this process has read, from `/proc/self/io`.
fn bytes_read() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("rchar: "))
                .and_then(|value| value.trim().parse().ok())
        })
        .unwrap_or(0)
}

#[test]
fn a_scan_reports_what_a_merge_would_reclaim_and_reads_no_data_block() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let source_dir = dir.join("NOPASS");
    if !source_dir.is_dir() {
        eprintln!("skipping: the compressed set is absent");
        return;
    }

    let before = bytes_read();
    let found = scan::scan(&source_dir).unwrap();
    let read = bytes_read() - before;

    assert_eq!(found.sets.len(), 1);
    let set = &found.sets[0];
    assert_eq!(set.members, 3);
    assert!(set.bytes > 3_000_000_000, "{} bytes", set.bytes);
    assert_eq!(set.problem, None);
    // One candidate per starting file: 0, 1 and 2 of a set whose newest file is 2.
    assert_eq!(set.candidates.len(), 2);
    assert_eq!(set.candidates[0].to, 2);
    assert!(set.candidates[0].reclaims >= set.candidates[1].reclaims);
    assert_eq!(set.candidates[0].kind, plan::MergeKind::SyntheticFull);

    // The set is 3.7 GB. A scan that reads a hundredth of that read no data block.
    assert!(
        read < set.bytes / 100,
        "the scan read {read} bytes of a {} byte set",
        set.bytes
    );
}

#[test]
fn a_file_that_does_not_parse_is_listed_and_the_scan_carries_on() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let real = dir.join("3DFB059161047C13-RealFAT32-00-00.mrimg");
    if !real.exists() {
        eprintln!("skipping: the single-file set is absent");
        return;
    }

    let work = tempfile::tempdir().unwrap();
    std::fs::copy(&real, work.path().join(real.file_name().unwrap())).unwrap();
    std::fs::write(work.path().join("nonsense.mrimgx"), b"not a backup file").unwrap();
    std::fs::write(work.path().join("notes.txt"), b"ignored").unwrap();

    let found = scan::scan(work.path()).unwrap();

    assert_eq!(found.sets.len(), 1, "the readable set is still reported");
    assert_eq!(
        found.skipped.len(),
        1,
        "and only the broken file is skipped"
    );
    assert!(found.skipped[0].path.ends_with("nonsense.mrimgx"));
    assert!(
        found.skipped[0].reason.contains("too short to hold"),
        "{}",
        found.skipped[0].reason
    );
}

#[test]
fn nothing_is_deleted_without_the_flag_and_everything_absorbed_is_deleted_with_it() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let source_dir = dir.join("Backup-Set-MP");
    let name = |n: u16| format!("584221F3840B0DBE-MP-Full-{n:02}-{n:02}.mrimg");
    if !source_dir.join(name(2)).exists() {
        eprintln!("skipping: the multi-partition set is absent");
        return;
    }

    // Copies, because this test deletes what it is given.
    let work = tempfile::tempdir().unwrap();
    for number in 0..=2 {
        std::fs::copy(
            source_dir.join(name(number)),
            work.path().join(name(number)),
        )
        .unwrap();
    }
    let member = |n: u16| work.path().join(name(n));

    // Without the flag the sources stay and the run says which are redundant. The output
    // goes to its own directory: a merged file left beside the files it absorbed makes two
    // members claim one file number, which the reader refuses.
    let elsewhere = tempfile::tempdir().unwrap();
    let quiet = elsewhere.path().join("MERGED-KEEP.mrimg");
    merge(&member(0), &member(2), &quiet);
    assert!(member(0).exists() && member(1).exists() && member(2).exists());

    let with_flag = work.path().join("MERGED-DELETE.mrimg");
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
        .arg("consolidate")
        .arg("--from")
        .arg(member(0))
        .arg("--to")
        .arg(member(2))
        .arg("--out")
        .arg(&with_flag)
        .arg("--delete-merged")
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // Every absorbed file is gone, and the output is not.
    for number in 0..=2 {
        assert!(!member(number).exists(), "file {number} should be deleted");
    }
    assert!(with_flag.is_file(), "the output survives");
    assert!(quiet.is_file(), "the earlier output was left alone");

    // The report names them oldest first, which is the order they were removed in.
    let report = String::from_utf8_lossy(&run.stdout);
    let order: Vec<usize> = (0..=2)
        .map(|n| report.find(&name(n as u16)).expect("each file is named"))
        .collect();
    assert!(order[0] < order[1] && order[1] < order[2], "{report}");
}

#[test]
fn the_end_to_end_test_hashes_every_block_of_an_uncompressed_output() {
    // The reference restore tests md5_hash only when compression is on, so an uncompressed
    // set gets no integrity test from it at all. This one tests every block.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let source_dir = dir.join("Backup-Set");
    let from = source_dir.join("DD5A77E6B68A6C34-Full-00-00.mrimg");
    let to = source_dir.join("DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !to.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }

    let out_dir = tempfile::tempdir().unwrap();
    let merged = out_dir.path().join("MERGED-00-00.mrimg");
    merge(&from, &to, &merged);

    let report = verify::verify_file(&merged, None).unwrap();
    assert_eq!(report.blocks, 237, "every block of the output is tested");
    assert_eq!(report.bytes, 15_532_032);
    assert_eq!(report.elsewhere, 0, "a synthetic Full holds all of its own");

    // A deliberately corrupted output is reported as a mismatch.
    let mut bytes = std::fs::read(&merged).unwrap();
    bytes[1024] ^= 0xff;
    std::fs::write(&merged, &bytes).unwrap();
    let error = verify::verify_file(&merged, None).unwrap_err().to_string();
    assert!(
        error.contains("does not match the hash the index records"),
        "{error}"
    );
}

#[test]
fn the_end_to_end_test_decrypts_and_decompresses_the_encrypted_set() {
    // The set is high compression and AES-128. Every block is decrypted, decompressed and
    // hashed, which is the only test that proves a copied block is still the block it was.
    //
    // The password is not in this repository. Put it in MRIMGX_TEST_PASSWORD to run this.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let Ok(password) = std::env::var("MRIMGX_TEST_PASSWORD") else {
        eprintln!("skipping: MRIMGX_TEST_PASSWORD is not set");
        return;
    };
    let source_dir = dir.join("PASS");
    let from = source_dir.join("E9A7F5B2D6166D7C-NOPASS-01-01.mrimgx");
    let to = source_dir.join("E9A7F5B2D6166D7C-NOPASS-02-02.mrimgx");
    if !to.exists() {
        eprintln!("skipping: the encrypted set is absent");
        return;
    }

    let out_dir = tempfile::tempdir().unwrap();
    let merged = out_dir.path().join("MERGED-02-02.mrimgx");
    merge(&from, &to, &merged);

    let report = verify::verify_file(&merged, Some(&password)).unwrap();
    assert_eq!(report.blocks, 8628);
    assert!(report.bytes > 500_000_000, "{} bytes", report.bytes);
    // Nothing is left untested: a delta index names only the positions the absorbed files
    // changed, and every one of those moved into the output.
    assert_eq!(report.elsewhere, 0);

    // A wrong password is reported as a wrong password, not as a corrupt block.
    let error = verify::verify_file(&merged, Some("not the password"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("the password is wrong"), "{error}");

    // And with no password at all, the run says what it needs.
    let error = verify::verify_file(&merged, None).unwrap_err().to_string();
    assert!(error.contains("needs its password"), "{error}");
}

#[test]
fn a_killed_merge_damages_nothing_and_recovery_clears_it() {
    // The failure that matters. A run killed part way through must leave every source byte
    // for byte as it was, no output in place, and its leftovers where a person can see them.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let source_dir = dir.join("Backup-Set-MP");
    let name = |n: &str| format!("584221F3840B0DBE-MP-Full-{n}.mrimg");
    if !source_dir.join(name("02-02")).exists() {
        eprintln!("skipping: the multi-partition set is absent");
        return;
    }

    // The sources are copies, so a failure damages nothing that matters.
    let work = tempfile::tempdir().unwrap();
    for part in ["00-00", "01-01", "02-02"] {
        std::fs::copy(source_dir.join(name(part)), work.path().join(name(part))).unwrap();
    }
    let member = |n: &str| work.path().join(name(n));
    let before: Vec<Vec<u8>> = ["00-00", "01-01", "02-02"]
        .iter()
        .map(|part| std::fs::read(member(part)).unwrap())
        .collect();

    let out_dir = tempfile::tempdir().unwrap();
    let out_path = out_dir.path().join("MERGED-00-00.mrimg");
    let set = BackupSet::discover(member("02-02")).unwrap();
    let expected = plan::build(&set, 0, 2).unwrap().projected_size();

    // What the chain restores to before any of this, to compare against afterwards.
    let baseline = refextract().map(|oracle| {
        let image = work.path().join("baseline.img");
        extract(&oracle, &member("02-02"), &image);
        image
    });

    for fraction in [0.1, 0.5, 0.9] {
        let written = kill_part_way(
            &member("00-00"),
            &member("02-02"),
            &out_path,
            fraction,
            expected,
        );
        assert!(
            written > 0 && written < expected,
            "killed after {written} of {expected} bytes, which is not part way through"
        );

        // Nothing is in place, and the leftovers are where a person can find them.
        assert!(!out_path.exists(), "a killed run leaves no output");
        assert!(
            out_dir.path().join("merge_running").exists(),
            "the lock survives, so the next run stops and says why"
        );
        let temp = leftover_temp(out_dir.path()).expect("the temporary file survives");

        // Every source is untouched, and the chain still resolves.
        let after: Vec<Vec<u8>> = ["00-00", "01-01", "02-02"]
            .iter()
            .map(|part| std::fs::read(member(part)).unwrap())
            .collect();
        assert_eq!(before, after, "a source changed during a killed run");
        let still = BackupSet::discover(member("02-02")).unwrap();
        assert_eq!(still.members.len(), 3);
        still.flatten().unwrap();

        // The independent extractor still restores the chain, which is the claim a person
        // actually cares about after a failed merge.
        if let Some(oracle) = refextract() {
            let image = work.path().join("after-kill.img");
            extract(&oracle, &member("02-02"), &image);
            if let Some(first) = &baseline {
                assert_eq!(
                    std::fs::read(&image).unwrap(),
                    std::fs::read(first).unwrap(),
                    "the chain extracts differently after a killed run"
                );
            }
        }

        // A second run refuses while the lock is there.
        let blocked = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
            .arg("consolidate")
            .arg("--from")
            .arg(member("00-00"))
            .arg("--to")
            .arg(member("02-02"))
            .arg("--out")
            .arg(&out_path)
            .output()
            .unwrap();
        assert!(!blocked.status.success());

        // Recovery clears both leftovers.
        let recovered = std::process::Command::new(env!("CARGO_BIN_EXE_mrimgx-consolidate"))
            .arg("consolidate")
            .arg("--recover")
            .arg("--from")
            .arg(member("00-00"))
            .arg("--to")
            .arg(member("02-02"))
            .arg("--out")
            .arg(&out_path)
            .output()
            .unwrap();
        assert!(recovered.status.success());
        assert!(!out_dir.path().join("merge_running").exists());
        assert!(!temp.exists(), "the temporary file is gone");
    }

    // And after all that, the merge runs and produces a file that reads back.
    merge(&member("00-00"), &member("02-02"), &out_path);
    let output = BackupFile::open(&out_path, true).unwrap();
    output.check_framing().unwrap();
    assert_eq!(output.header.merged_files, vec![0, 1]);
}

#[test]
fn a_merge_of_a_merge_still_resolves_and_extracts() {
    // Re-consolidation is the case the alias closure exists for. The first output claims
    // file numbers 0 and 1, so the second merge has to move blocks that are still tagged 0.
    // A range test over file numbers would leave those behind and then delete the file that
    // holds them.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let Some(oracle) = refextract() else {
        eprintln!("skipping: build the oracle with scratch/build-refextract.sh");
        return;
    };
    let source_dir = dir.join("Backup-Set-MP");
    let name = |n: &str| format!("584221F3840B0DBE-MP-Full-{n}.mrimg");
    if !source_dir.join(name("02-02")).exists() {
        eprintln!("skipping: the multi-partition set is absent");
        return;
    }

    let work = tempfile::tempdir().unwrap();
    for part in ["00-00", "01-01", "02-02"] {
        std::fs::copy(source_dir.join(name(part)), work.path().join(name(part))).unwrap();
    }
    let member = |n: &str| work.path().join(name(n));

    // First merge: files 0 and 1. The two sources are then removed, as a person would.
    let first = work.path().join("MERGED-FIRST.mrimg");
    merge(&member("00-00"), &member("01-01"), &first);
    std::fs::remove_file(member("00-00")).unwrap();
    std::fs::remove_file(member("01-01")).unwrap();

    // Second merge: that output with file 2.
    let second = work.path().join("MERGED-SECOND.mrimg");
    merge(&first, &member("02-02"), &second);
    std::fs::remove_file(&first).unwrap();
    std::fs::remove_file(member("02-02")).unwrap();

    let output = BackupFile::open(&second, true).unwrap();
    assert_eq!(output.header.file_number, 2);
    assert_eq!(output.header.merged_files, vec![0, 1]);
    assert!(!output.header.delta_index, "it carries a complete index");
    // It claims every number of the chain it replaced, so it is a set on its own.
    let set = BackupSet::discover(&second).unwrap();
    assert_eq!(set.members.len(), 1);

    let from_chain = work.path().join("chain.img");
    extract(&oracle, &source_dir.join(name("02-02")), &from_chain);
    let from_merge = work.path().join("merge.img");
    extract(&oracle, &second, &from_merge);

    compare_images(&from_chain, &from_merge);
}

#[test]
fn the_merge_extracts_to_the_same_image_as_the_chain() {
    // The gold standard. A second implementation, by different authors, restores the
    // original chain and the merged file. Validating our writer with our own reader proves
    // nothing, so this is the test that decides whether the merge is correct.
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let Some(oracle) = refextract() else {
        eprintln!("skipping: build the oracle with scratch/build-refextract.sh");
        return;
    };
    let target = dir.join("Backup-Set/DD5A77E6B68A6C34-Full-01-01.mrimg");
    if !target.exists() {
        eprintln!("skipping: the two-file set is absent");
        return;
    }

    let work = tempfile::tempdir().unwrap();
    let from_chain = work.path().join("chain.img");
    extract(&oracle, &target, &from_chain);

    let set = BackupSet::discover(&target).unwrap();
    let plan = plan::build(&set, 0, 1).unwrap();
    let name = "DD5A77E6B68A6C34-Full-01-01.mrimg";
    let merged = work.path().join(name);
    {
        let mut source = write::SourceFiles::open(&set, &plan).unwrap();
        let file = std::fs::File::create(&merged).unwrap();
        let mut out = std::io::BufWriter::new(file);
        write::write_output(&set, &plan, &mut source, &mut out, name).unwrap();
        out.into_inner().unwrap().sync_all().unwrap();
    }

    let from_merge = work.path().join("merge.img");
    extract(&oracle, &merged, &from_merge);

    // The two images are deliberately not the same length, and the numbers are pinned here
    // so that the disk_size trap cannot be mistaken for a difference in content.
    //
    // The extractor creates the image at disk_size of the file it was given, then writes
    // blocks, which extends the file when a block ends past that size. The chain ends at
    // file 1, an Incremental, whose disk_size is the CHS product of 534643200. Its last
    // block ends at 534773760, so the image stops there. The merge is a synthetic Full and
    // takes the true device size of 536870912 from the Full, which is past every block, so
    // the image is exactly that long. The extra 2097152 bytes lie beyond the partition.
    let chain_bytes = std::fs::read(&from_chain).unwrap();
    let merge_bytes = std::fs::read(&from_merge).unwrap();
    assert_eq!(chain_bytes.len(), 534_773_760);
    assert_eq!(merge_bytes.len(), 536_870_912);

    assert_eq!(
        chain_bytes,
        merge_bytes[..chain_bytes.len()],
        "the merged image differs from the chain image"
    );
    assert!(
        merge_bytes[chain_bytes.len()..].iter().all(|b| *b == 0),
        "the tail past the end of the chain image is not empty"
    );
}

/// Copy the whole encrypted set and report throughput.
///
/// Ignored by default: it moves about 3.8 GB and needs that much free space. Run it with
/// `cargo test --release --test corpus -- --ignored --nocapture` when the number matters.
#[test]
#[ignore]
fn merging_a_large_set_runs_at_disk_speed() {
    let Some(dir) = corpus() else {
        eprintln!("skipping: testdata/ is absent");
        return;
    };
    let target = dir.join("PASS/E9A7F5B2D6166D7C-NOPASS-02-02.mrimgx");
    if !target.exists() {
        eprintln!("skipping: the encrypted set is absent");
        return;
    }

    let set = BackupSet::discover(&target).unwrap();
    let plan = plan::build(&set, 0, 2).unwrap();

    // Write next to the corpus so the measurement reflects the real disk rather than a
    // memory-backed temporary directory.
    let out_path = dir.join("merge-throughput.tmp");
    let started = std::time::Instant::now();
    {
        let mut source = write::SourceFiles::open(&set, &plan).unwrap();
        let file = std::fs::File::create(&out_path).unwrap();
        let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
        let region = write::write_data_region(&plan, &mut source, &mut out, 2).unwrap();
        out.into_inner().unwrap().sync_all().unwrap();
        assert_eq!(region.payload_bytes, plan.bytes_to_copy());
    }
    let elapsed = started.elapsed();
    let _ = std::fs::remove_file(&out_path);

    let bytes = plan.bytes_to_copy();
    let rate = bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0;
    eprintln!(
        "copied {bytes} bytes in {:.2} s, {rate:.0} MB per second, {} blocks",
        elapsed.as_secs_f64(),
        plan.blocks_to_copy()
    );
    assert!(rate > 50.0, "throughput fell to {rate:.0} MB per second");
}
