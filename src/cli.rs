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
use crate::scan as scanner;
use crate::set::BackupSet;
use crate::verify;
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
        /// After writing, decrypt and decompress every block of the output and compare it
        /// against the hash the index records. An encrypted set needs its password in
        /// MRIMGX_PASSWORD.
        #[arg(long)]
        verify_md5: bool,
        /// Delete the files the output absorbed, once the output has been read back.
        #[arg(long)]
        delete_merged: bool,
        /// Report as JSON, so a scheduled job can act on it.
        #[arg(long)]
        json: bool,
    },
    /// Report which backup sets in a directory can be consolidated, and what that saves.
    Scan {
        /// The directory to look in.
        #[arg(value_name = "DIRECTORY")]
        directory: PathBuf,
        /// Report as JSON, so a scheduled job can act on it.
        #[arg(long)]
        json: bool,
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
        Command::Scan { directory, json } => scan(&directory, json),
        Command::Resolve { file, blocks } => resolve(&file, blocks),
        Command::Consolidate {
            from,
            to,
            out,
            dry_run,
            recover,
            verify_md5,
            delete_merged,
            json,
        } => consolidate(
            &from,
            &to,
            out.as_deref(),
            dry_run,
            recover,
            verify_md5,
            delete_merged,
            json,
        ),
    }
}

fn consolidate(
    from: &Path,
    to: &Path,
    out: Option<&Path>,
    dry_run: bool,
    recover: bool,
    verify_md5: bool,
    delete_merged: bool,
    json: bool,
) -> Result<()> {
    if recover {
        let directory = out
            .or(Some(to))
            .and_then(Path::parent)
            .unwrap_or(Path::new("."));
        let removed = commit::clear_leftovers(directory)?;
        if removed.is_empty() {
            println!("nothing to recover in {}", directory.display());
        } else {
            println!("cleared what a killed run left in {}:", directory.display());
            for path in removed {
                println!("    {}", path.display());
            }
        }
        return Ok(());
    }

    // The two-file rules run first, so that a From file newer than the To file is reported
    // in the documented wording rather than as a file missing from the set.
    let from_file = BackupFile::open(from, false)?;
    let to_file = BackupFile::open(to, false)?;
    plan::check_pair(&from_file, &to_file)?;

    // The set is resolved as of the To file, so discovery starts there.
    let set = BackupSet::discover(to)?;
    let plan = plan::build(
        &set,
        from_file.header.file_number,
        to_file.header.file_number,
    )?;

    // In JSON, a run that writes reports one document at the end, with the plan inside it.
    // Two documents on one stream would not parse.
    if !json {
        print_plan(&set, &plan);
    } else if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan_as_json(&set, &plan))?
        );
    }

    if dry_run {
        return Ok(());
    }
    let out = out
        .context("give --out FILE to write the merge, or --dry-run to report what it would move")?;
    write_merge(&set, &plan, out, verify_md5, delete_merged, json)
}

/// The plan as a document, carrying the same numbers the text form prints.
fn plan_as_json(set: &BackupSet, plan: &plan::MergePlan) -> serde_json::Value {
    serde_json::json!({
        "imageid": set.newest().header.imageid,
        "from": plan.from,
        "to": plan.to,
        "kind": plan.kind.consolidation_type(),
        "absorbs": plan.redundant_file_numbers(),
        "out_file_number": plan.out_file_number,
        "out_increment_number": plan.out_increment_number,
        "blocks_to_copy": plan.blocks_to_copy(),
        "bytes_to_copy": plan.bytes_to_copy(),
        "blocks_kept": plan.blocks_kept(),
        "holes": plan.holes(),
        "projected_size": plan.projected_size(),
        "redundant_files": plan.redundant_file_numbers().iter()
            .filter_map(|n| set.owner(*n).map(|f| f.path.display().to_string()))
            .collect::<Vec<_>>(),
    })
}

/// Report every backup set in a directory and what merging it would save.
fn scan(directory: &Path, json: bool) -> Result<()> {
    let found = scanner::scan(directory)?;

    if json {
        let document = serde_json::json!({
            "directory": directory.display().to_string(),
            "sets": found.sets.iter().map(|set| serde_json::json!({
                "imageid": set.imageid,
                "newest": set.newest.display().to_string(),
                "members": set.members,
                "bytes": set.bytes,
                "problem": set.problem,
                "candidates": set.candidates.iter().map(|c| serde_json::json!({
                    "from": c.from,
                    "to": c.to,
                    "kind": c.kind.consolidation_type(),
                    "moves": c.moves,
                    "reclaims": c.reclaims,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "skipped": found.skipped.iter().map(|s| serde_json::json!({
                "path": s.path.display().to_string(),
                "reason": s.reason,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }

    if found.sets.is_empty() {
        println!("no backup set in {}", directory.display());
    }
    for set in &found.sets {
        if let Some(problem) = &set.problem {
            println!(
                "set {}  {} files, {} bytes: {problem}",
                set.imageid, set.members, set.bytes
            );
            continue;
        }
        let best = &set.candidates[0];
        if best.reclaims == 0 {
            println!(
                "set {}  {} files, {} bytes: nothing to gain from a merge",
                set.imageid, set.members, set.bytes
            );
            continue;
        }

        println!(
            "set {}  {} files, {} bytes, newest {}",
            set.imageid,
            set.members,
            set.bytes,
            set.newest.display()
        );
        for candidate in &set.candidates {
            let kind = match candidate.kind {
                MergeKind::SyntheticFull => "synthetic full",
                MergeKind::IncrementalMerge => "incremental merge",
            };
            println!(
                "    files {} through {}  {kind:<18} moves {} bytes, reclaims {} bytes",
                candidate.from, candidate.to, candidate.moves, candidate.reclaims
            );
        }
    }

    for skipped in &found.skipped {
        println!("skipped {}: {}", skipped.path.display(), skipped.reason);
    }
    Ok(())
}

/// Write the merge, commit it, and read it back.
fn write_merge(
    set: &BackupSet,
    plan: &plan::MergePlan,
    out: &Path,
    verify_md5: bool,
    delete_merged: bool,
    json: bool,
) -> Result<()> {
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
    // Everything the run wants to say, collected so that the text form and the JSON form
    // carry the same words.
    let mut notes: Vec<String> = mount.caveats();

    // Refuse now rather than forty gigabytes in.
    let free = commit::check_free_space(directory, plan.projected_size())?;

    let held = format!(
        "merging files {} through {}\noutput {}",
        plan.from,
        plan.to,
        out.display()
    );
    let _lock = commit::Lock::take(directory, &held)?;

    let mut source = write::SourceFiles::open(set, plan)?;
    let mut temp = commit::TempOutput::create(out)?;
    // The output replaces the files it absorbs, so it carries their permissions.
    if let Some(model) = set.owner(plan.to) {
        temp.take_permissions_from(&model.path)?;
    }
    let reserved = temp.reserve(plan.projected_size());
    if reserved == commit::Reservation::Unsupported {
        notes.push("this file system does not reserve space in advance".to_string());
    }
    let written = {
        let mut writer = BufWriter::new(temp.writer());
        let written = write::write_output(set, plan, &mut source, &mut writer, name)?;
        writer.flush()?;
        written
    };
    let committed = temp.commit(&mount, written.size)?;
    if committed.flush_downgraded {
        notes.push("the strong flush is not supported here, so a weaker one was used".to_string());
    }
    if committed.rename_retries > 0 {
        notes.push(format!(
            "the rename needed {} retries",
            committed.rename_retries
        ));
    }
    if committed.rename_error_was_wrong {
        notes.push("the rename reported an error for work it had already done".to_string());
    }

    // Over a network mount only one confirmation is worth trusting: re-open the output and
    // read it back.
    write::check_output(out, plan)?;

    let verified = if verify_md5 {
        // The password never comes from the command line, because arguments show up in the
        // process list.
        let password = std::env::var("MRIMGX_PASSWORD").ok();
        Some(verify::verify_file(out, password.as_deref())?)
    } else {
        None
    };

    // Only now, after the output was read back, may a source be removed. A rename that
    // returned success is not enough, because on a network mount it does not prove much.
    let redundant = redundant_paths(set, plan);
    if delete_merged {
        for path in &redundant {
            std::fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))?;
        }
    }

    if json {
        let document = serde_json::json!({
            "plan": plan_as_json(set, plan),
            "output": out.display().to_string(),
            "bytes": written.size,
            "index_file_position": written.data.end,
            "destination": mount.describe(),
            "free_space": free,
            "space_reserved": reserved == commit::Reservation::Made,
            "read_back": true,
            "verified": verified.as_ref().map(|v| serde_json::json!({
                "blocks": v.blocks,
                "bytes": v.bytes,
                "not_tested": v.elsewhere,
            })),
            "deleted": delete_merged,
            "redundant_files": redundant.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            "notes": notes,
        });
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }

    println!();
    println!("destination      {} on {}", out.display(), mount.describe());
    println!("  free space     {free} bytes");
    println!("wrote {} ({} bytes)", out.display(), written.size);
    println!("  read back and checked against the plan");
    for note in &notes {
        println!("  note           {note}");
    }
    if let Some(verified) = &verified {
        println!(
            "  verified       {} blocks, {} bytes of plaintext, every hash matched",
            verified.blocks, verified.bytes
        );
        if verified.elsewhere > 0 {
            println!(
                "  not tested     {} blocks that still live in another file of the set",
                verified.elsewhere
            );
        }
    }
    if delete_merged {
        println!("  deleted the files the output absorbed, oldest first:");
    } else {
        println!("  these files are now redundant:");
    }
    for path in &redundant {
        println!("    {}", path.display());
    }
    if !delete_merged {
        println!("  nothing was deleted. Pass --delete-merged to remove them");
    }
    Ok(())
}

/// The files the output makes redundant, in ascending file number order.
///
/// One file can answer for several numbers, because a member that was consolidated before
/// claims every number it absorbed. Each path is listed once.
fn redundant_paths(set: &BackupSet, plan: &plan::MergePlan) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for number in plan.redundant_file_numbers() {
        if let Some(owner) = set.owner(number) {
            if !paths.contains(&owner.path) {
                paths.push(owner.path.clone());
            }
        }
    }
    paths
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
