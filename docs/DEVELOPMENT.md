# Development

## What you need

Rust 1.96 or later. Nothing else for the library tests.

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## The tests that need real backup files

`tests/corpus.rs` runs against backup files in a gitignored `testdata/` directory. If the
files a test wants are absent, that test prints one line and skips. So a fresh clone is green
and proves less than a working checkout does.

The sample corpus comes from Macrium's own repository:

```sh
git clone --filter=blob:none --no-checkout --branch linux-contrib-tidy \
    https://github.com/macrium/mrimgx_file_layout.git /tmp/mrimgx-corpus
cd /tmp/mrimgx-corpus
git sparse-checkout set --no-cone 'contrib/extract-to-img/Backup-Files/*'
git checkout
cp -r contrib/extract-to-img/Backup-Files/* <repo>/testdata/
```

Those files are uncompressed and unencrypted, and they carry the `.mrimg` extension although
their contents are the format Reflect X writes as `.mrimgx`. Compressed and encrypted sets
have to come from a Reflect installation of your own.

## The independent extractor

The strongest tests compare an extraction of the merged file against an extraction of the
chain, using Macrium's own `contrib/extract-to-img`. It is a separate implementation by
different authors, so a bug here and a bug there do not cancel out.

`scratch/build-refextract.sh` builds it and records the two changes it makes to the upstream
sources. The tests look for the binary at `/tmp/refextract`, or wherever `REFEXTRACT` points.
Without it, the tests that need it skip.

That extractor handles neither compression nor encryption, so it works on the sample corpus
only.

## Passwords in tests

The end-to-end test of an encrypted set needs its password, and no password lives in this
repository. Put it in `MRIMGX_TEST_PASSWORD` to run that test. Without it, the test skips.

The tool itself reads a password from `MRIMGX_PASSWORD`, never from the command line.

## Running one thing

```sh
cargo test --lib                       # the fast tests, no backup files needed
cargo test --test corpus               # the tests that use real files
cargo test --test corpus the_merge     # one of them
cargo test -- --ignored --nocapture    # the heavy throughput run
```

## Where to look first

`docs/ARCHITECTURE.md` describes the file format and the modules. The module documentation in
`src/` carries the detail, and every constant that came from measuring a real file says which
file and what was measured.

## A word about the working notes

A gitignored `scratch/` directory holds format notes, plans and one-off probes. Nothing in it
is part of the build, and nothing derived from a disassembler belongs anywhere else.
