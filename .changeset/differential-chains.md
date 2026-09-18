---
default: patch
---

#### Refuse an incremental merge across a Differential, and accept retention gaps

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
