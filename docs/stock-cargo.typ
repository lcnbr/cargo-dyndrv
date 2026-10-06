= Stock Cargo backend

This fork can drive an unmodified Cargo executable and run its actual compiler
commands through Nix. Cargo owns feature resolution, command construction,
crate scheduling, fingerprints, build-script execution and test selection. The
backend binds compiler inputs to immutable store objects, runs the compiler,
and delivers its outputs to the same Cargo target directory.

The implemented entry points are `capture-cargo`, `nix-cargo` and
`snapshot-references`. They do not require a patched Cargo, a custom unit-graph
export or a Nix source patch. The validated Nix mode uses a separately owned
Linux worker and private daemon. Packaging that worker inside an ordinary Nix
builder is a separate integration boundary; enabling the existing overlay does
not provide this lifecycle automatically.

== Invoke the existing workload

Build the tool with the workspace's pinned dependencies. Select absolute Cargo
and rustdoc executables from the same toolchain as rustc. Each invocation requires
a new absolute session directory whose parent already exists; the session stores
wrappers, observations and terminal records. Reusing a session directory is an
error. Keep one coherent target for the desired Cargo command sequence and use a
new session for each command.

Native observation runs the supplied Cargo arguments directly:

```sh
cargo-dyndrv capture-cargo \
  --directory /tmp/worker/session-native-build \
  --cargo /absolute/toolchain/bin/cargo \
  --rustdoc /absolute/toolchain/bin/rustdoc \
  -- build --offline --locked
```

After the worker has prepared its private daemon, inputs and backend configuration,
use the same arguments through Nix:

```sh
cargo-dyndrv nix-cargo \
  --directory /tmp/worker/session-nix-build \
  --cargo /absolute/toolchain/bin/cargo \
  --rustdoc /absolute/toolchain/bin/rustdoc \
  --nix-config /tmp/worker/backend.json \
  -- build --offline --locked
```

The example paths must be replaced with the worker's actual paths. Run from the
intended workspace and supply its existing features, profiles, targets, jobs and
compiler environment. The driver exclusively owns `RUSTC_WRAPPER`; an inherited
nonempty value is rejected. Preserve Clippy's own wrapper chain. The rustdoc
wrapper observes doctests and runs their compilation and execution natively on
each call. Tests and build scripts remain Cargo processes. A cached compiler
output does not count as test execution.

The driver is not a workload definition: the application must still request
Python preparation, its test inventory, examples, Clippy, doctests and other
required checks. GammaLoop uses one authoritative runner for its Cargo and Nix
comparisons. Optional package, documentation and WASM publication can remain
outside required-check completion.

== Backend configuration and worker ownership

`backend.json` is a strict JSON object. Unknown fields are rejected. The current
fields are:

#table(
  columns: (1.3fr, 3fr),
  table.header([Field], [Contract]),
  [`nix`], [Absolute Nix executable in the store.],
  [`socket`], [Explicit private daemon socket with a private parent directory.],
  [`real_store`], [Absolute physical root containing that daemon's `nix/store`; logical compiler paths remain under `/nix/store`.],
  [`source`], [Normalized source root under `/tmp` or `/build`.],
  [`target`], [Normalized Cargo target under `/tmp` or `/build`; it must not contain the source or traverse symlinks.],
  [`package_sources`], [Optional additional, disjoint package-source roots, including declared vendored or external sources. Defaults to an empty list.],
  [`inputs`], [Nonempty store input roots for the compiler, linker, native tools/libraries and builder runtime. Provision their closures before compilation.],
  [`environment_names`], [Explicit environment values admitted to compiler inputs.],
  [`omitted_environment_names`], [Explicitly omitted environment names; disjoint from the forwarded names.],
  [`incremental`], [Whether dyndrv owns rustc incremental children. Defaults to false; Cargo's incremental settings must agree.],
  [`incremental_observer`], [Diagnostic observation of reused compiler products. Defaults to false; measure its cost separately.],
  [`source_overlay`], [Experimental private Linux overlay mounts for immutable source trees. Defaults to false; source and dependency verification remain enabled.],
  [`private_target`], [Experimental ordinary storage for the empty target placeholder beneath a source overlay. Defaults to false and requires `source_overlay`. The setting enters each immutable compiler recipe.],
  [`jobserver_socket_directory`], [Optional private directory for the experimental jobserver transport. Omitted or null disables transport.],
)

The worker owns daemon creation, tool/native-input provisioning, source layout,
resource limits, runtime credentials, measured Cargo execution and daemon cleanup.
Use a Cargo home outside the imported source tree, and keep the logical compiler
paths stable when moving a physical worker. The backend validates source and
artifact ownership rather than accepting arbitrary external paths as dependencies.
Runtime-generated inputs and declared build-script output directories must satisfy
that same ownership contract.

With `private_target`, the driver bind-mounts an exclusively created empty
directory over the source image's empty target placeholder. The target keeps its
original compiler-visible path. Nonempty generated trees, file outputs and stdout
scratch directories retain their existing treatment and verification; they are
never hidden by this mount. The namespace belongs to the single-threaded compiler
builder and disappears when it exits. External target directories already use
ordinary storage. This prototype needs local mount, compiler and full-workload
validation before adoption; existing snapshots remain bound to their original
driver and policy. It does not change Cargo, Nix or cache delivery.

The validated daemon configuration enables `nix-command` and `ca-derivations`,
uses a private local store and socket, and preserves the host daemon's settings.
Per-compiler requests use `nix build --offline` with substitution disabled. The
worker must therefore provision tools and downloaded compiler-state backing paths
explicitly. Remote cache delivery and publication are measured worker stages;
invoking `nix-cargo` does not configure NixCI authentication or publish a snapshot.

Compiler environment values enter immutable requests. Keep runtime licenses and
cache credentials out of that environment policy and out of generated inputs.
Supply credentials to the worker only when their runtime operation needs them.
The observation records store environment names, not arbitrary values. Raw
`CARGO_MAKEFLAGS` cannot be included in `environment_names`.

== Preserve Cargo's jobserver across the daemon

Enable `jobserver_socket_directory` only with the matching worker setup. The
directory must already exist, be normalized, owned by the worker and private,
and be disjoint from every source and target tree. The private daemon must expose
it at `/dyndrv-jobserver` inside compiler sandboxes, for example with its own
`extra-sandbox-paths` configuration. This mode has been validated with a same-user
private daemon; a normal multi-user daemon or another remote-worker arrangement
needs its own validation.

The driver duplicates Cargo's actual jobserver pipe descriptors and transfers them
through a Unix socket to the compiler. It preserves the token pool instead of
creating an independent compiler budget. A request's immutable identity includes
whether transport is enabled; physical socket paths and descriptor numbers are
transport details. Missing or mismatched endpoints fail the invocation instead
of silently falling back to native compilation.

A request lock prevents two live brokers from owning the same endpoint. Normal
cleanup removes the socket. After a crashed owner, a new lock owner can reclaim
its stale socket. Empty lock-marker files may remain; do not remove them while
an owner is live. Neither those markers nor descriptors belong in compiler-state
snapshots. Existing same-user controls cover actual pipe use, concurrent compilers,
interruption, exact-output hits and writable incremental rebuilding. Full workload
performance and deployment acceptance require separate measurements.

== Compiler state, immutable outputs and test results

Keep three cases distinct:

+ Cargo considers an unchanged crate fresh and never invokes its compiler.
+ The wrapper is invoked and an exact Nix compiler result already exists locally.
  The compiler does not execute for that result.
+ Changed source invokes the compiler, which consumes compatible incremental
  state and reuses products from a previous revision.

The third case requires evidence from the changed crate. Successful tests, a fast
build, unchanged-crate reuse or an exact-output hit cannot establish it. Compare
independent B workers restored from A with and without incremental data, verify
that B outputs were initially absent, and retain actual compiler and reuse
receipts. Required tests and doctests must still run when testing execution cost.

Enable Cargo/rustc incremental compilation and dyndrv's `incremental` policy
together, with explicit, matching codegen units and other compiler settings.
The backend owns incremental children alongside ordinary outputs. Target
`.dyndrv-delivered` records associate those files with their immutable backing;
copying target artifacts while dropping the records does not preserve ownership.

After a successful Nix Cargo session, collect its backing references:

```sh
cargo-dyndrv snapshot-references \
  --directory /tmp/worker/session-nix-build \
  --output /tmp/worker/new-snapshot-references.json
```

The destination must be new. Collection validates the completed session and
current target ownership without importing source, contacting the daemon or
building missing outputs; it also works after the daemon stops. It lists backing
paths and ownership counts. It does not archive the target, sign an output, upload
it, select a previous revision or restore a writable target.

Wait for the application's required checks and a quiescent target before publishing
accepted state. An application snapshot owner must perform those separate operations: identify
compatible source/workload, toolchain, native inputs, driver policy and compiler
settings; retain the selected ordinary artifacts, incremental data and ownership
records; include their backing closure in an immutable output; publish it; and
verify and restore it writable on the receiving worker. Missing or incompatible
snapshots must not cause an old producer to rebuild merely to prepare B.
GammaLoop's identity also separates enabled jobserver policy from its physical
socket directory. A changed driver/helper requires a newly compatible producer.

Measure required-check completion, compilation by context, worker CPU, artifact
sizes, restoration, export and publication separately. Ordinary local targets can
contain runtime-generated profile artifacts omitted from a snapshot; their
retention difference can cause additional compilation after download. Keep
compiler state out of runtime-only test delivery. A passing local worker proves
neither CI cache authentication nor the cost of an additional deployment layer.

== Completion and failure records

Nix sessions write observations under `invocations/` and a terminal
`cargo-result.json`. Compiler receipts preserve the original compiler status;
completed negative capability probes remain distinct from a backend failure or
an incomplete delivery. A successful Cargo exit is accepted only when every
non-query compiler invocation completed delivery and every doctest completed its
native stage. If a build script swallows an incomplete backend invocation, the
target is marked rejected. Preserve that evidence and restore verified state or
use an empty target before trying again.

The observations and receipts support audits of exact outputs, native doctests,
compiler inputs and optional incremental witnesses. They are diagnostic records,
not a guarantee that an arbitrary workload or cache deployment has been tested.
The older unit-graph inspection/execution commands have their own contracts and
remain documented separately in the repository README.
