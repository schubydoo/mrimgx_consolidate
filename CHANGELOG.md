# Changelog

This project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

knope writes each entry from the files in `.changeset/` at the release.

## 0.1.1 (2026-09-18)

### Features

#### `scan --recursive` scans every folder below a path ([#5](https://github.com/schubydoo/mrimgx_consolidate/pull/5))

`scan --recursive` (or `-r`) runs the scan in the folder you name and in every folder below
it. The text form prints only the folders that hold a backup file. A total line follows: the
number of sets, how many can be merged, and the bytes the best merge of each reclaims. The
`--json` form lists every folder.

The walk does not follow links to folders, so a link loop cannot trap it. It does not enter
the snapshot folders `.zfs`, `.snapshot` and `#snapshot`, which hold copies of the same sets.
A folder that cannot be listed is reported as skipped, and the walk carries on.

### Fixes

#### Refuse an incremental merge across a Differential, and accept retention gaps ([#4](https://github.com/schubydoo/mrimgx_consolidate/pull/4))

An incremental merge whose range held a Differential dropped the changes that only the
Differential records. The merged file then restored the wrong bytes and reported no error. The
tool now refuses that range and names the Differential. A merge from the Full was never
affected, because it resolves every block. Do not use version 0.1.0 for an incremental merge
of a set that holds a Differential.

A set that Reflect's retention thinned out, for example a Full, two Differentials and the
Incrementals after the newer one, was reported as not complete. The check now asks for the
files a restore actually reads: the newest Full or Differential, every file its index names,
and every file after it.

`scan` now lists every range it refuses, with the reason, instead of leaving it out.

The README now carries a warning: keep a separate copy of every backup set before you run
`consolidate`.

## 0.1.0 (2026-09-18)

### Features

#### The first release: consolidate Macrium Reflect X backup sets on Linux, macOS and FreeBSD ([#2](https://github.com/schubydoo/mrimgx_consolidate/pull/2))

What it does:

- `consolidate` merges a range of a backup set into one file. It copies every block byte for
  byte, so nothing is decompressed and nothing is re-encrypted, and an encrypted set needs no
  password.
- `scan` reports which sets in a directory can be merged and what that reclaims. It reads
  metadata only.
- `resolve` reports where every logical block of a set lives.
- `inspect` prints the header, the layout and the block counts of a file.
- `--dry-run` reports what a merge moves and writes nothing.
- `--verify-md5` decrypts, decompresses and hashes every block of the output, and compares
  each one against the hash the index records.
- `--delete-merged` removes the files the output absorbed, oldest first, after the read-back.
- `--recover` clears the lock and the temporary file a killed run left behind.
- `--json` reports as a document, for a scheduled job.

How it keeps a backup safe:

- A source file is never opened for writing.
- The output goes to a temporary file in the destination directory under a random name, then
  a rename, then a read-back. Nothing is deleted before that read-back.
- The destination is classified as local or remote before the write starts. On a network
  mount a rename error triggers a re-read of both paths, and a permission or busy error is
  retried with backoff.
- Free space is measured and reserved before the copy starts. A FAT32 destination that cannot
  hold the output is refused.
- Two files that claim one file number are refused. Picking between them is a guess, and a
  guess about where a block lives is not acceptable here.

What was measured:

- All six From and To pairs of the four-file, three-partition sample set extract to the image
  the chain extracts to, using Macrium's own extractor.
- Macrium Reflect X 10.0.8843 opens a merged file, groups it into its backup set, mounts it,
  and verifies it with no complaint.
- 8628 blocks of an AES-128 set decrypt, decompress and match their recorded hashes.
- A run killed at ten, fifty and ninety percent leaves every source byte for byte as it was.
