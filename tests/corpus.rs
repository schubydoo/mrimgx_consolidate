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
use mrimgx_consolidate::reader::BackupFile;
use mrimgx_consolidate::set::BackupSet;

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
