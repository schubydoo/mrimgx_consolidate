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

A change that a user can see also needs a changeset, which is a short file in `.changeset/`
that becomes its changelog entry. Run `knope document-change` to write one. A test, a refactor
or a documentation fix needs none. The file looks like this:

```markdown
---
default: minor
---

#### `scan --json` reports the reclaimed bytes per candidate

The JSON form of `scan` now carries `reclaims` for each candidate.
```

Use `major` for a change that breaks a documented behavior, `minor` for a new capability, and
`patch` for a fix. The release takes the largest bump among the pending files.

## Releases

A maintainer cuts a release in three steps:

1. Run `gh workflow run knope-release.yml` to rehearse the build and the signing without a publish.
2. On an up-to-date `main`, run `GITHUB_TOKEN=$(gh auth token) knope prepare-release`.
3. Review and merge the `chore: prepare release X.Y.Z` pull request that it opens.

For the first release, add `--override-version 0.1.0` to step 2. Without a previous tag, knope
bumps the version in `Cargo.toml` past it.

The first step bumps the version from the pending changesets, writes `CHANGELOG.md`, and opens
the pull request from a branch named `release`. It runs locally because a pull request opened by
the workflow token starts no workflow run, so continuous integration never reports on it.

The merge starts `.github/workflows/knope-release.yml`. It builds static Linux binaries for
amd64 and arm64, macOS binaries for arm64 and x86_64, and a FreeBSD amd64 binary. It then signs
the checksums with cosign, attaches SLSA build provenance, and publishes the GitHub release with
every asset attached. A missing asset stops the run before anything is tagged.

## Style

`cargo fmt` decides formatting. `cargo clippy --all-targets -- -D warnings` has to pass.

Comments explain why, not what. If a constant came from measuring a real file, the comment
says which file and what was measured. Write documentation and messages in plain English:
short sentences, active voice, and no word that a person outside this field has to look up.
