= Stock Cargo worker

`cargo-dyndrv-worker` packages the Linux lifecycle around `nix-cargo`: editable
Cargo source discovery, stable source namespaces, private Cargo targets, optional
private daemons, shared-system-store access, GC roots and executable launchers.
It does not select an application's test inventory or build profiles. Applications
construct their ordinary Cargo, Nextest or other configured command arguments.

The flake exports `packages.<system>.cargo-dyndrv-worker`; the overlay exports
`cargo-dyndrv-worker` alongside the compiler driver. The package runs the source
and worker unit tests during its build. Its `testsSource` attribute retains the
generic host qualification suites separately from the installed worker modules.

== Invocation

```sh
cargo-dyndrv-worker --bootstrap /absolute/bootstrap \
  --workspace /absolute/checkout --action check -- \
  check --locked --profile dev
cargo-dyndrv-worker --bootstrap /absolute/bootstrap \
  --workspace /absolute/checkout --action clean --
```

The bootstrap directory provides `environment.json` and `tools/bin`.
The tools include Bash, flock, Cargo source-inventory tools (Git/JJ), Nix and
the application's native build tools. Its registered Nix references must retain
the complete tools and runtime closure; provisioning copies that closure to a
private store. The worker leaves the caller's checkout
unchanged except for explicitly exported launchers, documentation and test reports
under `target/`.
The closure must retain the worker package itself: namespace children and
exported launchers invoke its installed script. The configured `python` must
provide `tomli_w` as well as the standard library.

The environment specification contains:

- `environment`: the pinned compiler and native-library environment.
- `inputs`: immutable compiler/native-tool store roots.
- `nix`, `cargo`, `rustdoc`, `driver`, `nextest`, `python`: absolute executables.
- `runtime_path`: executable search path for the Cargo session and source staging.
- `vendor_expression`: a Nix expression producing a Cargo vendor directory with
  `config.toml`. Its lockfile is supplied as `CARGO_DYNDRV_CARGO_LOCK`; the worker
  strips declared private values before invoking Nix. The vendor provider can use
  Crane or another application-selected implementation.
- `runtime_environment_names` and `runtime_environment_prefixes`: application
  values forwarded to runtime processes but omitted from compiler requests.
- `private_environment_names`: runtime values also excluded from Nix operations
  and metadata discovery. These names automatically join the runtime omission
  policy. Supply credentials at runtime, never in this JSON or a derivation.
- `forward_environment_names`: additional application-specific compiler inputs.
  Standard Cargo profile, Rust compiler and build-job controls are already handled.
- `workspace_environment_names`: variables rebound to the staged workspace for
  runtime tools and exported launchers; defaults are Nextest and Insta roots.
- `action_executables`: optional application action names mapped to session
  executables, such as a Maturin packaging command. Their arguments are supplied
  unchanged after `--`, with declared manifest paths remapped into the stage.
- `post_commands`: optional action-to-command-list mapping, run after successful
  session execution while its workspace lock remains held. Command arguments may
  contain literal `{workspace}` and `{bootstrap}` placeholders. The original,
  remapped session arguments are appended to each command. Application export
  hooks belong here; they execute as the caller and are not compiler derivations.

Compiler tools and native libraries should have a closure separate from session
tools, worker code and export hooks. Updating a session tool need not invalidate
matching immutable compiler outputs. Complete effective compiler inputs, including
incremental state, still determine reuse; no end-to-end speedup is promised.

== State and source contract

`CARGO_DYNDRV_STATE` selects the private state directory, defaulting to
`${XDG_STATE_HOME:-$HOME/.local/state}/cargo-dyndrv`. State directories must be
owned, mode `0700`, and outside the selected source roots. Workspaces and build
policies have separate targets and locks; immutable compiler outputs can be shared.
Failed compiles retain completed dependency outputs and incremental backing.
Interrupted/rejected targets require the explicit `clean` action before reuse.

`CARGO_DYNDRV_STORE` selects `private` or `system`. The optional
`CARGO_DYNDRV_STORE_CONFIG` overrides `/etc/cargo-dyndrv/store.json`. A system
configuration has `version = 1`, `store_mode = "system"` and an administrator-owned
`logical_root` (default `/build/cargo-dyndrv`). System mode uses the existing Nix
daemon without granting the caller trusted-user status. `gc` and `stop` refuse to
collect or stop that shared service. In private mode they manage only the owned
daemon/store. `CARGO_DYNDRV_OBSERVE_INCREMENTAL=1` enables diagnostic object-reuse
evidence; its extra hashing is excluded from performance claims.

Cargo metadata and declared manifest/configuration paths discover local sources,
including external workspaces and unused patches. Each root must belong to Git
or JJ. Dirty selected files and new nonignored modules are copied with stable
timestamps; secrets, VCS state, build products and virtual environments are
excluded. Safe selected symlinks are materialized; escaping, ignored and special
files are rejected before modifying the old stage. Path declarations are rewritten
for the stable namespace. The worker does not mutate the original manifests.

Compiler requests retain their owning source root, dependency source roots and
declared generated/additional inputs. Procedural macros can read their dependency
assets during downstream expansion. Crates sharing a source root invalidate
together; implicit undeclared cross-root reads are outside the source contract.
Vendored packages have stable individual logical paths, so an unused lockfile
addition does not rename another package's compiler source.

== Qualification

The generic suites under `tests/worker/` require a prepared bootstrap and run on
the host, outside a nested Nix build sandbox:

```sh
python3 tests/worker/test_integration.py --bootstrap /absolute/bootstrap
python3 tests/worker/test_invalidation.py --bootstrap /absolute/bootstrap
python3 tests/worker/test_macro_sources.py --bootstrap /absolute/bootstrap
python3 tests/worker/test_incremental.py --bootstrap /absolute/bootstrap
python3 tests/worker/test_system.py --bootstrap /absolute/bootstrap --second-user USER
```

They cover source relocation, runtime identity and credentials, fresh test
execution, concurrent clients, compiler failure/GC, SIGKILL recovery, source and
session-tool invalidation, macro assets and incremental reuse/corruption. The
system suite additionally needs the configured shared daemon and a second normal
UID. Evidence directories retain logs and isolated fixtures for review; owned
private daemons stop on completion or failure. `workload.py` is a test fixture
that constructs a small Cargo/Nextest workload, not application command dispatch.
Applications can subclass the integration harness's invocation and fixture-copy
methods to run these same contracts through their own real entry point.

For qualification, the specification also names `worker` (the packaged CLI),
`worker_script` (the installed Python script, used to identify an owned process)
and `compiler_closure` (the compiler-only closure identity). The bootstrap exposes
`compiler-environment.json`, `compiler-closure`, `closure` and `worker-tests` for
identity and lifetime assertions. These extra metadata links are not required by
the worker's runtime interface.
