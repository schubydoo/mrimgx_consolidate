---
default: minor
---

#### The first release: consolidate Macrium Reflect X backup sets on Linux, macOS and FreeBSD

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
