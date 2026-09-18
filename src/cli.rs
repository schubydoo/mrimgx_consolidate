//! Command line surface.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};

use crate::commit;
use crate::index::Blocks;
use crate::json;
use crate::plan::{self, MergeKind};
use crate::reader::BackupFile;
use crate::set::BackupSet;
use crate::write;

#[derive(Parser, Debug)]
#[command(name = "mrimgx-consolidate", version, about, long_about = None)]
#[command(arg_required_else_help = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
#[non_exhaustive] // More subcommands are coming; adding one must not be a breaking change.
pub enum Command {
    /// Parse a backup file and print what it holds.
    Inspect {
        /// One or more `.mrimg` or `.mrimgx` files.
        #[arg(required = true, value_name = "FILE")]
        files: Vec<PathBuf>,
    },
    /// Merge a range of a backup set into one file.
    Consolidate {
        /// The first file of the range. Usually the Full.
        #[arg(long, value_name = "FILE")]
        from: PathBuf,
        /// The last file of the range. It must be the newest file of the set.
        #[arg(long, value_name = "FILE")]
        to: PathBuf,
        /// Where to write the merged file. Required unless `--dry-run` is given.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Report the plan and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Clear a lock left behind by a killed run, then stop.
        #[arg(long)]
        recover: bool,
    },
    /// Resolve a backup set and report where every logical block lives.
    Resolve {
        /// Any file of the set. The set is resolved as of this file.
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Print one line per block. Large sets produce a lot of output.
        #[arg(long)]
        blocks: bool,
    },
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Inspect { files } => inspect(&files),
        Command::Resolve { file, blocks } => resolve(&file, blocks),
        Command::Consolidate {
            from,
            to,
            out,
            dry_run,
            recover,
        } => consolidate(&from, &to, out.as_deref(), dry_run, recover),
    }
}

fn consolidate(
    from: &Path,
    to: &Path,
    out: Option<&Path>,
    dry_run: bool,
    recover: bool,
) -> Result<()> {
    if recover {
        let directory = out
            .or(Some(to))
            .and_then(Path::parent)
            .unwrap_or(Path::new("."));
        return match commit::Lock::clear(directory)? {
            true => {
                println!("cleared the lock in {}", directory.display());
                Ok(())
            }
            false => {
                println!("no lock in {}", directory.display());
                Ok(())
            }
        };
    }

    // The set is resolved as of the To file, so discovery starts there.
    let set = BackupSet::discover(to)?;
    let from_file = BackupFile::open(from, false)?;
    let to_file = BackupFile::open(to, false)?;
    let plan = plan::build(
        &set,
        from_file.header.file_number,
        to_file.header.file_number,
    )?;

    print_plan(&set, &plan);

    if dry_run {
        return Ok(());
    }
    let out = out
        .context("give --out FILE to write the merge, or --dry-run to report what it would move")?;
    write_merge(&set, &plan, out)
}

/// Write the merge, commit it, and read it back.
fn write_merge(set: &BackupSet, plan: &plan::MergePlan, out: &Path) -> Result<()> {
    for member in &set.members {
        ensure!(
            !is_same_file(&member.path, out),
            "the output path {} is a file of this backup set. \
             A merge never writes over a file it reads",
            out.display()
        );
    }
    ensure!(
        !out.exists(),
        "{} already exists. This tool never writes over a file that is there",
        out.display()
    );
    let name = out
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{} has no usable file name", out.display()))?;
    let directory = out.parent().unwrap_or(Path::new("."));

    // Classified before anything is written, because it decides what a rename is worth.
    let mount = commit::Mount::of(directory);
    println!();
    println!("destination      {} on {}", out.display(), mount.describe());
    for caveat in mount.caveats() {
        println!("  note           {caveat}");
    }

    // Refuse now rather than forty gigabytes in.
    let free = commit::check_free_space(directory, plan.projected_size())?;
    println!("  free space       {free} bytes");

    let note = format!(
        "merging files {} through {}\noutput {}",
        plan.from,
        plan.to,
        out.display()
    );
    let _lock = commit::Lock::take(directory, &note)?;

    let mut source = write::SourceFiles::open(set, plan)?;
    let mut temp = commit::TempOutput::create(out)?;
    // The output replaces the files it absorbs, so it carries their permissions.
    if let Some(model) = set.owner(plan.to) {
        temp.take_permissions_from(&model.path)?;
    }
    if temp.reserve(plan.projected_size()) == commit::Reservation::Unsupported {
        println!("  note           this file system does not reserve space in advance");
    }
    let written = {
        let mut writer = BufWriter::new(temp.writer());
        let written = write::write_output(set, plan, &mut source, &mut writer, name)?;
        writer.flush()?;
        written
    };
    let committed = temp.commit(&mount, written.size)?;

    // Over a network mount only one confirmation is worth trusting: re-open the output and
    // read it back.
    write::check_output(out, plan)?;

    println!();
    println!("wrote {} ({} bytes)", out.display(), written.size);
    println!("  read back and checked against the plan");
    if committed.flush_downgraded {
        println!("  the strong flush is not supported here, so a weaker one was used");
    }
    if committed.rename_retries > 0 {
        println!("  the rename needed {} retries", committed.rename_retries);
    }
    if committed.rename_error_was_wrong {
        println!("  the rename reported an error for work it had already done");
    }
    println!("  these files are now redundant:");
    for number in plan.redundant_file_numbers() {
        if let Some(owner) = set.owner(number) {
            println!("    file {number:>3}  {}", owner.path.display());
        }
    }
    println!("  nothing was deleted. This tool never removes a source file");
    Ok(())
}

/// Whether two paths name the same file, decided before the second one exists.
fn is_same_file(existing: &Path, planned: &Path) -> bool {
    let resolved = |path: &Path| -> Option<PathBuf> {
        let parent = path.parent().unwrap_or(Path::new("."));
        Some(parent.canonicalize().ok()?.join(path.file_name()?))
    };
    match (resolved(existing), resolved(planned)) {
        (Some(a), Some(b)) => a == b,
        _ => existing == planned,
    }
}

fn print_plan(set: &BackupSet, plan: &plan::MergePlan) {
    let kind = match plan.kind {
        MergeKind::SyntheticFull => "a synthetic full",
        MergeKind::IncrementalMerge => "an incremental merge",
    };
    println!("backup set {}", set.newest().header.imageid);
    println!("  merge            files {} through {}", plan.from, plan.to);
    println!("  produces         {kind}");
    println!(
        "  output claims    file {} increment {}",
        plan.out_file_number, plan.out_increment_number
    );
    println!(
        "  absorbs          {}",
        plan.redundant_file_numbers()
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  blocks to copy   {}  ({} bytes)",
        plan.blocks_to_copy(),
        plan.bytes_to_copy()
    );
    println!("  blocks kept      {}", plan.blocks_kept());
    println!("  holes            {}", plan.holes());
    println!("  projected size   {} bytes", plan.projected_size());

    let current: u64 = plan
        .redundant_file_numbers()
        .iter()
        .filter_map(|n| set.owner(*n))
        .map(|f| f.size)
        .sum();
    if current > 0 {
        let projected = plan.projected_size();
        println!("  files absorbed   {current} bytes on disk");
        if current > projected {
            println!("  reclaims         {} bytes", current - projected);
        } else {
            println!("  reclaims         nothing. The merge grows the set");
        }
    }

    for part in &plan.partitions {
        let copies = part
            .reserved
            .iter()
            .chain(part.blocks.iter())
            .filter(|a| a.is_copy())
            .count();
        let reserved_copies = part.reserved.iter().filter(|a| a.is_copy()).count();
        println!(
            "  disk {} partition {}  {} entries, {copies} copied ({reserved_copies} reserved)",
            part.disk,
            part.partition,
            part.blocks.len()
        );
    }

    println!();
    println!("  These files become redundant once the output is in place:");
    for number in plan.redundant_file_numbers() {
        if let Some(owner) = set.owner(number) {
            println!("    file {number:>3}  {}", owner.path.display());
        }
    }
}

fn resolve(path: &PathBuf, list_blocks: bool) -> Result<()> {
    let set = BackupSet::discover(path)?;
    let flat = set.flatten()?;

    println!("backup set {}", set.newest().header.imageid);
    println!("  resolved as of  {}", set.newest().path.display());
    println!("  members         {}", set.members.len());
    for member in &set.members {
        let h = &member.header;
        println!(
            "    file {:>3}  inc {:>3}  {:<5} {:<6}  {}",
            h.file_number,
            h.increment_number,
            h.backup_type,
            if h.is_full_index() { "full" } else { "delta" },
            member.path.display()
        );
    }

    let counts = flat.blocks_per_file();
    let mut owners: Vec<_> = counts.iter().collect();
    owners.sort();
    println!(
        "  live blocks     {}  ({} bytes)",
        flat.blocks().count(),
        flat.stored_bytes()
    );
    for (file_number, count) in owners {
        let owner = set
            .owner(*file_number)
            .map(|f| f.path.display().to_string())
            .unwrap_or_else(|| "NO OWNER".to_string());
        println!("    from file {file_number:>3}  {count:>8} blocks  {owner}");
    }

    for (d, disk) in flat.disks.iter().enumerate() {
        for (p, blocks) in disk.iter().enumerate() {
            let holes = blocks.iter().filter(|e| e.is_hole()).count();
            println!(
                "  disk {d} partition {p}  {} logical blocks  {} captured  {holes} holes",
                blocks.len(),
                blocks.len() - holes
            );
            if list_blocks {
                for (i, e) in blocks.iter().enumerate() {
                    if e.is_hole() {
                        continue;
                    }
                    println!(
                        "    {d}/{p}/{i} file={} at={} len={}",
                        e.file_number, e.file_position, e.block_length
                    );
                }
            }
        }
    }
    Ok(())
}

fn inspect(files: &[PathBuf]) -> Result<()> {
    for path in files {
        let file = BackupFile::open(path, true)?;
        file.check_framing()?;
        print_report(&file)?;
        println!();
    }
    Ok(())
}

fn print_report(file: &BackupFile) -> Result<()> {
    let h = &file.header;
    println!("{}", file.path.display());
    println!("  size                 {}", file.size);
    println!("  imageid              {}", h.imageid);
    println!(
        "  file / increment     {} / {}",
        h.file_number, h.increment_number
    );
    println!(
        "  backup_type          {}{}",
        h.backup_type,
        if h.is_full_index() {
            "  (holds a full index)"
        } else {
            "  (delta index)"
        }
    );
    println!("  split_file           {}", h.split_file);
    println!(
        "  merged_files         {}",
        if h.merged_files.is_empty() {
            "none".to_string()
        } else {
            format!("{:?}", h.merged_files)
        }
    );
    println!(
        "  compression          {}",
        setting(file, "_compression", "compression_level")
    );
    println!(
        "  encryption           {}",
        setting(file, "_encryption", "aes_type")
    );
    println!("  index_file_position  {}", h.index_file_position);
    println!("  root list at         {}", file.root_at);
    println!(
        "  own data ends at     {}  (gap before the index region: {})",
        file.own_data_end(),
        h.index_file_position as i64 - file.own_data_end() as i64
    );

    // The canonical round-trip is what makes patching the metadata safe. Report it here so
    // a file that would refuse to consolidate says so at inspect time instead.
    match json::check_round_trip(&file.json, &file.json_raw) {
        Ok(()) => println!("  json round-trip      exact"),
        Err(err) => println!("  json round-trip      MISMATCH: {err}"),
    }

    for (d, disk) in file.disks.iter().enumerate() {
        let names: Vec<String> = disk
            .blocks
            .blocks
            .iter()
            .map(|b| b.header.name_str())
            .collect();
        println!("  disk {d}  metadata: {}", names.join(" "));
        for (p, part) in disk.partitions.iter().enumerate() {
            let (kind, count) = match &part.index.blocks {
                Blocks::Full(v) => ("full", v.len()),
                Blocks::Delta(v) => ("delta", v.len()),
            };
            let holes = match &part.index.blocks {
                Blocks::Full(v) => v.iter().filter(|e| e.is_hole()).count(),
                Blocks::Delta(v) => v.iter().filter(|d| d.element.is_hole()).count(),
            };
            println!(
                "    partition {p}  reserved {}  {kind} blocks {count}  holes {holes}",
                part.index.reserved.len()
            );
        }
    }
    Ok(())
}

fn setting(file: &BackupFile, object: &str, key: &str) -> String {
    file.json
        .get(object)
        .and_then(|o| o.get(key))
        .map(|v| match v.as_str() {
            Some(s) => s.to_string(),
            None => v.to_string(),
        })
        .unwrap_or_else(|| "absent".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn inspect_needs_at_least_one_file() {
        assert!(Cli::try_parse_from(["mrimgx-consolidate", "inspect"]).is_err());
    }

    #[test]
    fn inspect_accepts_several_files() {
        let cli =
            Cli::try_parse_from(["mrimgx-consolidate", "inspect", "a.mrimg", "b.mrimg"]).unwrap();
        let Command::Inspect { files } = cli.command else {
            panic!("expected the inspect subcommand");
        };
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn a_short_file_is_rejected_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.mrimg");
        std::fs::write(&path, b"not a backup").unwrap();
        assert!(BackupFile::open(&path, true).is_err());
    }

    #[test]
    fn block_name_constants_are_eight_bytes() {
        for name in [
            block::JSON,
            block::BITMAP,
            block::FAT,
            block::CBT,
            block::MFT,
            block::TRACK0,
            block::INDEX,
            block::EPT,
            block::AUXDATA,
        ] {
            assert_eq!(name.len(), 8, "{:?}", std::str::from_utf8(name));
        }
    }
}
