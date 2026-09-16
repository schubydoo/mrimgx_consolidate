//! Command line surface.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::index::Blocks;
use crate::json;
use crate::reader::BackupFile;
use crate::set::BackupSet;

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
