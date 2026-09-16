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

use std::path::{Path, PathBuf};

use mrimgx_consolidate::block;
use mrimgx_consolidate::index::Blocks;
use mrimgx_consolidate::json;
use mrimgx_consolidate::plan;
use mrimgx_consolidate::reader::BackupFile;
use mrimgx_consolidate::set::BackupSet;
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
