# Contributing

This tool rewrites other people's backups. That one fact sets the bar for everything below.

## Before you start

Open an issue first for anything larger than a typo. Say what you want to change and why. A
patch that arrives without one is welcome, but it can be turned down for a reason that one
message settles in advance.

Read `README.md` for what the tool does and where it stands.

## Setting up

You need Rust 1.96 or later. Nothing else.

```sh
git clone https://github.com/schubydoo/mrimgx_consolidate
cd mrimgx_consolidate
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The library tests run without any backup files. The tests in `tests/corpus.rs` need real
ones. If those files are absent, each of those tests skips rather than fails.
`docs/DEVELOPMENT.md` explains how to fetch a corpus. It also explains how to build the
independent extractor that the strongest tests compare against.

## The rules that are not style

Break these and the change does not go in, however good the idea is.

**A source file is never opened for writing.** Not to fix a field. Not to update an index.
Not to save a recovery file beside it. The tool writes one new file and touches nothing else.

**Nothing is deleted before the run read the output back.** A rename that returns success is
not proof on a network mount.

**The crate contains no `unsafe` code.** A call that needs it gets a safe wrapper, or the
feature stays out.

**A block is copied byte for byte.** Nothing is decompressed, re-encrypted or re-encoded on
the way through. A change that makes the tool re-encode a block needs a very good reason.

**A refusal beats a guess.** A real file sometimes says something this crate does not model.
Stop there, and say so in words a person can act on.

## Tests

Every change that fixes a bug comes with a test that fails without the fix.

Prove the instrument before you trust it. A test that passes because it measured nothing is
worse than no test. If a test asserts that something is absent, make it show that the same
check finds the thing when it is there.

Say what a test measured rather than what it assumed. Numbers from real files belong in the
test as constants, with a comment on where they came from.

## Commits and pull requests

One change per pull request. Use Conventional Commits for the subject line:

```
feat(write): copy the metadata region and rebuild each $INDEX
fix(reader): handle compressed metadata blocks
test(corpus): merge every pair, and the compressed and encrypted sets
```

Branch from the default branch, and open the pull request against it. In the body, say what
changed, what you measured, and what you did not test. A pull request that says "not tested
on macOS" is easier to trust than one that stays quiet.

Expect review to ask for evidence. "It works" is not evidence. The output of the test you
added is.

## Style

`cargo fmt` decides formatting. `cargo clippy --all-targets -- -D warnings` has to pass.

Comments explain why, not what. If a constant came from measuring a real file, the comment
says which file and what was measured. Write documentation and messages in plain English:
short sentences, active voice, and no word that a person outside this field has to look up.
