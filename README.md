# mrimgx-consolidate

Consolidate Macrium Reflect X backup sets from Linux, FreeBSD or macOS.

Macrium Reflect writes `.mrimgx` backup files. Over time a backup set grows into a Full
plus a long chain of Incrementals. Merging that chain back into one file is called
consolidation, and Macrium ships a tool for it called `consolidate.exe`. That tool runs
only on Windows. If the backup set lives on a network share, you must boot a Windows
machine and drive the whole merge across the network.

This tool does the same job on the machine that holds the files.

## Status

Early. The reader works. The writer does not exist yet.

What works today:

- `inspect` parses a backup file and prints its header, layout and block counts.
- Full, Incremental and delta indexes, multiple disks, and multiple partitions.

What does not work yet:

- Consolidation itself.
- Compressed and encrypted backup sets. The reader refuses them rather than guessing.
- Split backup files.

## Install

Build from source. You need Rust 1.96 or later.

```sh
cargo build --release
```

The binary lands at `target/release/mrimgx-consolidate`.

## Usage

```sh
mrimgx-consolidate inspect BACKUP-00-00.mrimg
```

## How it works

The file format is published by Macrium under the MIT license at
[macrium/mrimgx_file_layout](https://github.com/macrium/mrimgx_file_layout). That
repository also holds a reference reader in C++. It has no writer.

A `.mrimgx` file stores its data blocks first and its metadata last, with a 20-byte footer
at the end pointing back at the metadata. Each partition carries a block index. For every
logical block of the file system, that index names the bytes that hold it. It also names
which file of the set those bytes live in. An Incremental stores only the changed blocks. For
everything else it stores index entries that point back at the earlier files.

Consolidation copies the blocks that are about to become unreachable into one new file and
writes a rewritten index. The copy is byte for byte. Nothing is decompressed and nothing
is re-encrypted. The format ties a block's encryption to its logical position rather than
to its offset in the file, and that position does not change.

## Safety

The tool never modifies a source file. It writes to a temporary file in the destination
directory, flushes it, and renames it into place. It deletes nothing unless you pass an
explicit flag, and then only after re-opening the output and re-reading it.

That read-back is not a formality. On an NFS or SMB mount, flushing a directory is a no-op
that reports success. A rename that returns an error on such a mount sometimes succeeded
anyway. So on a network share the read-back is the only trustworthy confirmation, and
deletion waits for it rather than for the rename.

## License

MIT. See [LICENSE](LICENSE).

This project is not affiliated with Macrium or with the company that publishes it.
