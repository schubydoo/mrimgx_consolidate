# mrimgx-consolidate

[![CI](https://github.com/schubydoo/mrimgx_consolidate/actions/workflows/ci.yml/badge.svg)](https://github.com/schubydoo/mrimgx_consolidate/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/schubydoo/mrimgx_consolidate/branch/main/graph/badge.svg)](https://codecov.io/gh/schubydoo/mrimgx_consolidate)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![MSRV 1.96](https://img.shields.io/badge/MSRV-1.96-blue)](https://blog.rust-lang.org/2026/05/28/Rust-1.96.0/)

Consolidate Macrium Reflect X backup sets from Linux, FreeBSD or macOS.

> [!WARNING]
> **You use this tool at your own risk. The authors accept no responsibility for data loss.
> Keep a separate, working copy of every backup set before you run `consolidate`.** This is
> an independent project. Macrium does not make or support it. The software comes with no
> warranty, as the MIT license states.

Macrium Reflect writes `.mrimgx` backup files. Over time a backup set grows into a Full plus
a long chain of Incrementals. Merging that chain back into one file is called consolidation,
and Macrium ships a tool for it that runs only on Windows. If the backup set lives on a
network share, that means booting a Windows machine and driving the whole merge across the
network.

This tool does the same job on the machine that holds the files.

## What it does

- Merges a range of a backup set into one file, from any member to the newest one.
- Copies every block byte for byte. Nothing is decompressed, and nothing is re-encrypted.
- Needs no password, even for an encrypted set, because it never decrypts anything.
- Writes a new file, renames it into place, and reads it back before it reports success.
- Deletes a source file on request, and then only after the read-back.

It handles uncompressed and compressed sets, AES-encrypted sets, several disks, several
partitions, and a merge of a file that was merged before.

It refuses five things, and names the reason each time:

- a split backup set
- a set that mixes compression or encryption settings
- a Differential as the From or To file
- an incremental merge across a Differential
- a destination that cannot hold the output

## What is proven, and how

The merge is measured against Macrium's own code rather than against this project's opinion
of it.

**An independent extractor restores the merged file to the image the chain restores to.** The
extractor is Macrium's own `contrib/extract-to-img`. It is a separate implementation by
different authors. All six From and To pairs of a four-file, three-partition set produce a
matching image.

**Macrium Reflect X accepts the merged file.** Reflect X 10.0.8843 on Windows opens a file
this tool merged and groups it into its backup set. It mounts the file as a drive letter. Its
`Verify Image` run reports no complaint.

**Every block of an output can be decrypted, decompressed and hashed.** The `--verify-md5`
option undoes everything the format did to each block. It then compares the result against
the hash the index records. On the AES-128 test set that is 8628 blocks, all matching.

**A killed run damages nothing.** The tool is killed at ten, fifty and ninety percent of the
copy. Every source file is then byte for byte as it was, and no output is in place.

## Install

Build from source. You need Rust 1.96 or later.

```sh
cargo build --release
```

The binary lands at `target/release/mrimgx-consolidate`.

## Usage

Find out what a folder of backups is worth merging:

```sh
mrimgx-consolidate scan /mnt/backups
```

```
set 584221F3840B0DBE  4 files, 51285734 bytes, newest /mnt/backups/...-03-03.mrimgx
    files 0 through 3  synthetic full     moves 44302336 bytes, reclaims 6692078 bytes
    files 1 through 3  incremental merge  moves 7536640 bytes, reclaims 4023102 bytes
```

Report what a merge moves, and write nothing:

```sh
mrimgx-consolidate consolidate --dry-run \
    --from /mnt/backups/SET-00-00.mrimgx \
    --to   /mnt/backups/SET-03-03.mrimgx
```

Run it:

```sh
mrimgx-consolidate consolidate \
    --from /mnt/backups/SET-00-00.mrimgx \
    --to   /mnt/backups/SET-03-03.mrimgx \
    --out  /mnt/backups/MERGED-00-00.mrimgx
```

The output claims the identity of the From file, which is what Macrium's own tool leaves
behind. Once the files it absorbed are gone, rename it to the name that file had.

Other things it can do:

- `--verify-md5` decrypts and hashes every block of the output afterwards. An encrypted set
  needs its password in `MRIMGX_PASSWORD`.
- `--delete-merged` removes the files the output absorbed, oldest first, after the read-back.
- `--json` reports as a document, for a scheduled job.
- `--recover` clears the lock and the temporary file a killed run left behind.
- `inspect FILE` prints a file's header, layout and block counts.
- `resolve FILE` reports where every logical block of a set lives.

## Safety

The tool never opens a source file for writing. It writes the output under a temporary name
in the destination directory, flushes it, renames it into place, and reads it back. It
deletes nothing unless you pass `--delete-merged`. Then it deletes only after that read-back.

The read-back is not a formality. On an NFS or SMB mount, flushing a directory does nothing
and still reports success. A rename that returns an error on such a mount sometimes succeeded
anyway. So on a network share the read-back is the only report worth trusting, and a deletion
waits for it rather than for the rename.

Before the copy starts, the run classifies the destination and measures its free space. It
then asks the file system to reserve that space. It prints which guarantees do not hold on
that destination.

## How it works

Macrium publishes the format under the MIT license at
[macrium/mrimgx_file_layout](https://github.com/macrium/mrimgx_file_layout), together with a
reference reader in C++. That reader has no writer. This crate is the missing writer.

A backup file stores its data blocks first and its metadata last, with a 20-byte footer that
points back at the metadata. Each partition carries a block index. That index covers every
logical block of the file system. For each one it names the bytes that hold it, and the file
of the set those bytes live in. An Incremental stores only the blocks it changed.

Consolidation copies the blocks that are about to become unreachable into one new file and
writes a rewritten index. The copy is byte for byte. A block's encryption is tied to its
logical position rather than to its offset in the file. A merge preserves that position, so a
copied block stays readable without a password.

`docs/ARCHITECTURE.md` goes further.

## Status

The merge works, measured against the reference extractor and against Reflect X.

Not done yet: continuous integration, signed release binaries, and publication to crates.io.
No merged image went back onto real hardware yet, although Reflect X mounts one and reports
it sound.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). The rules that are not style, such as never writing
to a source file, are listed there.

## License

MIT. See [LICENSE](LICENSE).

This project is not affiliated with Macrium or with the company that publishes it.
