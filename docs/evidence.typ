= Packaging retained evidence

`cargo-dyndrv-evidence` seals existing files and their completion receipts into a
self-contained ZIP. It runs no compiler, test, scientific checker, or subprocess.
The worker package installs it alongside `cargo-dyndrv-worker`; from a checkout,
use `python3 worker/evidence.py` with the same arguments.

```sh
cargo-dyndrv-evidence package --spec evidence-spec.json \
  --root /path/to/retained-run --output /path/to/new-packet.zip \
  > packaging-receipt.json
cargo-dyndrv-evidence validate /path/to/new-packet.zip
```

The output parent must exist and the output must not exist. Success exits zero
and prints a JSON receipt; errors exit nonzero. Use the exit status before
consuming a redirected receipt. Nothing is uploaded. The existing native
publication helper remains responsible for Nix output submission.

== Declare the boundary once

The specification contains an explicit relative file list, a scope description,
bindings into retained JSON, and producer completion checks. The five record
groups are required; each contains one or more named bindings. An empty pointer
selects a whole JSON document. Otherwise pointers use JSON Pointer syntax:
`/array/0`, `~1` for a slash in a key, and `~0` for a tilde.

For a producer whose retained `result.json` already has the following fields:

```json
{
  "version": 1,
  "scope": "Completed observation; scientific and review status remain in result.json",
  "files": ["result.json", "junit.xml", "stderr"],
  "records": {
    "source": {"identity": {"file": "result.json", "pointer": "/source"}},
    "run": {"identity": {"file": "result.json", "pointer": "/run"}},
    "compiler": {"outcome": {"file": "result.json", "pointer": "/compiler"}},
    "tests": {"outcome": {"file": "result.json", "pointer": "/tests"}},
    "timing": {"original": {"file": "result.json", "pointer": "/timing"}}
  },
  "checks": [
    {"file": "result.json", "pointer": "/complete", "equals": true}
  ]
}
```

Adapt the pointers to the existing producer's schema; do not generate a new
scientific result just to satisfy this example. Record the frozen commit/tree,
worker session, actual test UUID and command/filter when available. Preserve
review status at capture alongside the run identity. Pin expected source,
session and UUID values in checks against each receipt that carries them, so
mixing receipts from different runs fails. An applicable completion check must
identify the producer's terminal, reaped command. Test-accounting and
compiler/runtime-binding certificates remain produced by their existing owners;
include those certificates and any required selected-count checks.

Checks compare decoded JSON with type-sensitive equality: `true`, `1`, and `1.0`
are distinct. Missing files, fields, empty record groups, duplicate JSON
keys, non-finite JSON values and failed checks reject the packet. All bound
JSON documents must be declared files. Present empty lists, objects, strings and
`null` values are preserved; their interpretation belongs to producer checks. File names are unique canonical relative
paths. Symlinks, special files, parent traversal and file/directory conflicts are
rejected; there are no glob patterns or implicit recursive imports.

Retain failed tests as failed tests. A completed failed observation can be
packaged successfully. Keep compiler outcomes and test outcomes in their
respective groups; preserve raw producer status fields without reinterpretation.
In particular, dyndrv `cargo-result.json` uses a raw process wait status, whereas
the worker's `last-run.json` has a command exit code. Neither alone establishes
test counts or compiler success. Zero compiler requests may legitimately have
no new `compiler-inputs.json`; describe reuse with the existing binding receipt.
Use an explicit existing record for unknown or not-run phases, rather than
inventing a measured duration or successful outcome.

== Integrity and publication

The archive contains `files/<relative-path>` with the exact original bytes and
`manifest.json`. The manifest contains the normalized specification, projected
records, and a sorted inventory of paths, lengths and SHA-256 values. It has no
self-hash. The command receipt supplies detached manifest and archive hashes.
The JSON serialization, file order, permissions and ZIP timestamps are fixed;
the same specification and bytes produce the same archive despite different
input mtimes, source directory or output name. Files are stored uncompressed;
the command makes no algebraic or semantic deduplication. Distinct declared
paths remain distinct entries even if their bytes are equal.

Packaging streams files into a private sibling temporary file, checks their
device/inode/mode/size/mtime/ctime before and after reading, verifies the completed
archive, and rechecks the source fingerprints. A final hard link publishes the
complete archive atomically and refuses any existing destination, including a
concurrent publisher. Ordinary failures remove the owned temporary file. A kill
or host crash may leave a hidden `.partial` file; it is never a published packet.
This requires a filesystem supporting hard links within the output directory.
Producers must finish and retain their inputs before packaging. These checks
detect ordinary concurrent edits; they are not a hostile-writer filesystem
snapshot protocol. The original files are read only.

Validation checks the exact archive inventory, lengths/hashes, projected records
and declared completion checks using only the archive. Archive verification and
hashing share one open file, with descriptor and path mutation checks. It does not open paths
mentioned inside retained JSON. Such references are external unless separately
included in the specification. A metadata-only slice must say so in its scope.
No generic manifest establishes scientific correctness, source authenticity,
compiler/test association, review acceptance, or a complete dependency closure.
Those claims require the producer's scoped certificates and review. An archive
hash can bind the delivered bytes to a separately trusted receipt; it is not a
signature. Consumers should require validation before using an archive.

== Timings

Original timing fields are copied without changing their units or scope. The
command receipt records UTC bounds, monotonic wall seconds and process CPU
seconds. Packaging additionally reports `copy_seconds` (copying, projection,
manifest writing and file sync) and `validation_seconds` (archive verification
and final source checks). These are nested within command wall time, not extra
native-work durations. Process startup and receipt printing fall outside the
internal clock. Packaging timestamps and durations stay outside the deterministic
archive. Unknown scientific CPU/manual time remains unknown.

== Retained-packet demonstration, 2 November 2026

The read-only demonstration used
`generation-feynkit-refresh/.local/feynkit-refresh/qed-qcd-six-post-migration-20261102`:
source `1da2700de7b4ac27907aaf4168bd457d2df6626f`, tree
`b90ec0ea7f8fe2f9395445b7ee01c47f08cc6ff3`, UUID
`1272eed1-e482-4e31-982a-2d43e5c8308f`. All 43 entries in its existing index were
verified, then packaged with the original index and checksum: 45 files,
63,881,085 payload bytes. The retained executable was copied as data and never
executed. Source bytes and stat fingerprints matched before and after.

Two package invocations took 0.347277 s and 0.301661 s including Python process
startup. Internal wall times were 0.289699 s and 0.245033 s; process CPU times
were 0.281959 s and 0.238553 s. Both produced the same 63,915,915-byte archive,
SHA-256 `8f3d53aaed1c7968a96b87265e6382eb3dbca83b5cfc7f4bd026d1138fc6352e`.
The duplicate archive was removed after equality verification. A separate
`validate` invocation also passed. One compiler request, six passing tests and
the original pending independent-review status remain separate retained facts.

Evidence is retained under `target/worker-overhead-20261102/`: the demonstration
driver `demonstrate_evidence.py`, `evidence-demo-spec.json`, `evidence-demo.zip`,
both package receipts, `evidence-validate.json`, `evidence-demo-results.json`,
and test logs. The exact reusable invocation from the task workspace is:

```sh
python3 worker/evidence.py package \
  --spec target/worker-overhead-20261102/evidence-demo-spec.json \
  --root ../generation-feynkit-refresh/.local/feynkit-refresh/qed-qcd-six-post-migration-20261102 \
  --output target/worker-overhead-20261102/new-evidence-demo.zip
python3 worker/evidence.py validate \
  target/worker-overhead-20261102/new-evidence-demo.zip
```

This measures mechanical packaging of completed retained evidence. It excludes
specification preparation, source/provenance reasoning, scientific work and
review. It is not a controlled speedup comparison against historical 30–120 s
hand-written assembly windows. Existing producers are not automatically migrated;
they can retain a reusable declaration and invoke this command after their own
completion checks. The generation workspace and active build state were untouched.
