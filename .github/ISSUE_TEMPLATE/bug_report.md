---
name: Bug report
about: The tool did something wrong
title: ''
labels: bug
assignees: ''
---

## What happened

## What you expected instead

## How to reproduce

The command you ran. Replace the paths if you prefer:

```sh
mrimgx-consolidate ...
```

What the tool printed, in full. If it refused, the wording of the refusal matters.

```
```

## The backup set

Run this and paste the output. It reads metadata only and prints no path from inside the
files:

```sh
mrimgx-consolidate scan /path/to/the/folder
```

- The version of Macrium Reflect that made it, where you know it
- Compressed: yes or no
- Encrypted: yes or no
- Where the files live: local disk, USB, NFS, SMB, other

## Your setup

- Version, from `mrimgx-consolidate --version`:
- Operating system and version:
- File system of the destination:

## Did anything get damaged

This tool never modifies or deletes a source file. If one changed, say so here first. That
moves the report to the front of the queue.
