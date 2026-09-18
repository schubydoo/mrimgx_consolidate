# Security policy

## Supported versions

| Version | Supported |
| --- | --- |
| The latest release | Yes |
| Anything older | No |

This project is early. Fixes go into the next release rather than into a patch of an older
one.

## Reporting a vulnerability

Do not open a public issue for a security problem.

Use GitHub's private vulnerability reporting on this repository, under the Security tab. If
you cannot use it, write to `schuuby@proton.me` instead.

Please include:

- What the problem is, and what an attacker gets from it.
- The steps to reproduce it. Attach a backup file or a fragment of one where that helps.
- The version, from `mrimgx-consolidate --version`, and the platform.

You get a first reply within seven days. If the report holds up, you get a fix or a plan
with a date, and credit in the release notes unless you ask otherwise.

## What counts here

This tool reads backup files that someone else can have made, so it treats every length and
every offset in a file as untrusted input. The following are security problems in this
project:

- A crafted backup file that makes the tool write outside the file it was told to write.
- A crafted backup file that makes the tool read or write out of bounds in memory.
- Any path where the tool modifies, truncates or deletes a source file that it was not asked
  to delete.
- A password read from the environment that reaches a log, a file, or the process list.

The following are not security problems, although they are still worth reporting as issues:

- A refusal to merge a set that the tool decides it cannot handle.
- A crash on a corrupt file that leaves every source file untouched.

## What the tool does with your data

It opens every source file read-only and never writes to one. It writes the merged output to
a temporary file in the destination directory, renames it into place, then reads it back. If
you pass `--delete-merged`, it deletes a source only after that read-back. Otherwise it
deletes nothing.

It makes no network connection, sends no telemetry and looks for no updates.

A password is needed only for `--verify-md5`, which is optional. It is read from the
`MRIMGX_PASSWORD` environment variable, never from the command line, because command line
arguments are visible to every process on the machine. It is never written anywhere.

The crate contains no `unsafe` code.
