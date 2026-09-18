# Architecture

## The file, in write order

A backup file is written front to back and read back to front.

```
offset 0     data blocks, all disks and partitions
             zero padding up to a 4096-byte boundary
IFP          per-disk list:      $TRACK0 [, $EPT] [, blocks this crate does not name]
             per-partition list: [$BITMAP] [, others] , $INDEX     (one list per partition)
ROOT         root list:          $JSON [, $AUXDATA]
EOF minus 20 footer:             u64 offset of the root list, then MACRIUM_FILE
```

A reader starts at the footer and walks the root list. It parses `$JSON`, then walks the
per-disk and per-partition lists that start at `_header.index_file_position`. The disk and
partition regions carry no counts of their own. The document has to be parsed first to know
how many of each to expect.

`$INDEX` is the important block. It holds one entry per logical block of the partition. Each
entry is 30 bytes: the offset in the file, the MD5 of the plaintext, the stored length, and
the number of the file that holds those bytes. An Incremental stores a delta form instead,
34 bytes, which adds the logical position each entry replaces.

## The modules

Dependencies run one way. The format modules know nothing about consolidation.

| Module | Holds |
| --- | --- |
| `block` | The footer, the 32-byte block headers, the lists, and zstd |
| `index` | The 30-byte and 34-byte elements, and the `$INDEX` payload |
| `json` | The canonical document, and the round-trip gate |
| `crypto` | Key derivation, the password test, the per-block initialization vector |
| `reader` | One parsed backup file |
| `set` | Discovery of a set, and flattening of its chain |
| `plan` | The rules, the copy set, and the estimates |
| `write` | The forward pass, the document patch, and the read-back check |
| `commit` | The lock, the temporary file, the flush, the rename |
| `verify` | The optional end-to-end hash test |
| `scan` | What a directory holds and what merging it reclaims |
| `cli` | Arguments, reporting and exit codes |

## What a merge does

1. Discover the set as of the To file. Flatten the chain to one dense array per partition.
2. Decide, for every entry, one of three outcomes. The block is a hole. The block keeps its
   existing reference, because the file that holds it survives. The block's bytes move,
   because the file that holds it is about to go.
3. Copy the bytes that move into a new file, front to back, and record where each one landed.
4. Copy the per-disk and per-partition metadata byte for byte, and write a fresh `$INDEX` for
   each partition.
5. Patch the document and write it, then the root list, then the footer.
6. Read the whole thing back before reporting success.

## Three facts the design rests on

**A block can be copied byte for byte.** Its encryption depends on four things: the image id,
the disk number, the partition number and the logical block index. A merge preserves all
four. The hash covers the plaintext, which does not change either. Only the offset in the
file and the file number change, and both live in the index entry rather than in the block.

**A range over file numbers is the wrong rule.** A file that was merged before claims every
number it absorbed. Index entries elsewhere in the set still name those numbers. So the set
of numbers a merge takes over is a closure: every member in the range contributes its own
number and every number it already claims.

**The document survives a round trip.** The `$JSON` payload is what `nlohmann::json::dump(4)`
writes with sorted keys, plus a newline. `serde_json` without `preserve_order` reproduces it
byte for byte. The writer re-serializes the untouched document and compares it against the source before it
patches anything. A future Reflect version that writes a shape this crate cannot reproduce
therefore stops the run instead of losing a field.

## What the output claims

The merged file takes over the identity of the From file: its number and its increment. Every
other number of the range goes into `_header.merged_files`, so an index entry anywhere in the
set still resolves. That is what Macrium's own tool leaves behind, and Reflect X groups that
shape into a backup set.

The per-partition history is rewritten to one entry per file that still exists. Two entries
naming one file make Reflect X ask for a file it already has.
