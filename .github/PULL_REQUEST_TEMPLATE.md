## What this changes

## Why

Where an issue exists, link it.

## What you measured

Not "it works". The numbers, or the test output.

```
```

## Checklist

- [ ] `cargo test` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] For a bug fix, a test that fails without the change
- [ ] No source file is opened for writing anywhere in this change
- [ ] Nothing new is deleted before the output is read back
- [ ] No `unsafe` code
- [ ] Blocks are still copied byte for byte, with no decompression and no re-encryption

## What you did not test

Say it here. A pull request that names the gap is easier to trust than one that stays quiet.

- Platforms not tried:
- Backup sets not tried, such as compressed, encrypted, multiple disks, split files:
