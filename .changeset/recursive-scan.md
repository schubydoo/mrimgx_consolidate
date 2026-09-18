---
default: minor
---

#### `scan --recursive` scans every folder below a path

`scan --recursive` (or `-r`) runs the scan in the folder you name and in every folder below
it. The text form prints only the folders that hold a backup file. A total line follows: the
number of sets, how many can be merged, and the bytes the best merge of each reclaims. The
`--json` form lists every folder.

The walk does not follow links to folders, so a link loop cannot trap it. It does not enter
the snapshot folders `.zfs`, `.snapshot` and `#snapshot`, which hold copies of the same sets.
A folder that cannot be listed is reported as skipped, and the walk carries on.
