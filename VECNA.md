# agent-governance-toolkit (Vecna fork) — pin policy and release recipe

**Not a general fork.** This repo exists to publish one thing: a signed set of
build artifacts for the Agent Control Specification (ACS) engine — the Rust
core under `policy-engine/core` — that Vecna's `warden` daemon links against
(as a Go static library) and that the executor imports (as a Python wheel).
Everything else in the monorepo (the .NET/Node/other-language SDKs, the
agent-governance-python packages, the CLIs) rides along unmodified and unused.

**Pin.** Branch `vecna` is the default branch and is pinned to upstream
`main` @ `46463ef8689433817fcc0c582a7881f515d4df15` (2026-08-26), the most
recent upstream `main` commit whose repo-wide `VERSION` file reads `5.0.0`.
Upstream has not cut a `v5.0.0` tag — `VERSION` at that commit is the closest
stable reference point. **We never track or merge upstream `main`** after
this pin; bumping means re-pinning deliberately (see "Bumping upstream"
below). Remote `upstream` (`microsoft/agent-governance-toolkit`) is fetch/push
configured by `gh repo fork` but nothing is ever pushed there.

**The changes**, all on top of the pin, all under `policy-engine/`:

1. `policy-engine/core/Cargo.toml`: added `staticlib` to `[lib] crate-type`
   (was `["lib", "cdylib"]`) so `cargo build --release` also emits
   `libagent_control_specification_core.a`. The existing `pub mod ffi;`
   (`policy-engine/core/src/ffi.rs`, unconditionally compiled, 18
   `#[no_mangle] pub unsafe extern "C"` functions: `acs_builder_*`,
   `acs_runtime_*`, `acs_validate_artifacts`, `acs_free_string`) is the C ABI
   the Go binding links against — nothing there was touched.
2. `policy-engine/core/cbindgen.toml`: new — a minimal cbindgen config
   (`language = "C"`, include guard `ACS_ENGINE_H`) that generates
   `acs_engine.h` from `ffi.rs`. `AcsBuilder`/`AcsRuntime` aren't `#[repr(C)]`;
   cbindgen renders them as opaque forward-declared structs, which is correct
   since the ABI only ever hands out pointers to them.
3. `policy-engine/sdk/python/pyproject.toml`: `[project].name` changed from
   `agent-control-specification` to `vecna_acs_engine`, `version` from
   `0.3.1b1` to `5.0.0` (the ACS SDK versions independently of the monorepo
   `VERSION`; we pin it to the repo-wide `5.0.0` we're building from so the
   wheel and the source pin agree). `[tool.maturin].module-name` updated to
   `vecna_acs_engine._native` to match.
4. `policy-engine/sdk/python/vecna_acs_engine/` — directory renamed from
   `agent_control_specification/` (this is `git mv`, not a copy: same code).
   `__init__.py` gained `__version__ = "5.0.0"`. Four absolute imports in
   `_client.py` (`from agent_control_specification import _native`, plus the
   matching error-message strings) were updated to `vecna_acs_engine` — they
   have to be, since they name the top-level package by string and the
   package no longer has that name. Nothing else inside the package (log
   logger names, the OTel meter name, a default guardrail-name string) was
   touched; those are cosmetic identifiers, not import paths.
5. `.github/workflows/vecna-release.yml` — new, see below.

The rename exists for one reason: a stock upstream `agent-control-specification`
wheel must never silently satisfy a `vecna_acs_engine` pin.

**Release recipe** (`.github/workflows/vecna-release.yml`, triggers on tags
matching `v*-vecna.*`):

- `wheels`: `PyO3/maturin-action` × 3 targets (`x86_64-unknown-linux-gnu` +
  `aarch64-unknown-linux-gnu`, both `manylinux: 2_28`; `aarch64-apple-darwin`
  native on `macos-14`), building `policy-engine/sdk/python`.
- `staticlib`: plain `cargo build --release --target <triple>` for
  `policy-engine/core` on native `ubuntu-latest` (amd64) and
  `ubuntu-24.04-arm` (arm64, GitHub-hosted, free on this public repo) — no
  cross-compilation needed.
- `header`: one `cbindgen` run (config above) producing `acs_engine.h`.
- `src`: `git archive` scoped to `policy-engine/tests/conformance/` (the
  conformance suite) and `policy-engine/policy/` (stock Rego libs under
  `lib/`, Cedar libs under `cedar-lib/`) → `acs-src.tar.gz`.
- `release`: downloads everything, renames to the fixed asset names below,
  writes `SHA256SUMS`, publishes one GitHub release named after the tag.

**Published asset names** (fixed — later Vecna builds pin against these
exact strings, not against whatever maturin/cargo would name them natively):

```
vecna_acs_engine-<version>-cp312-cp312-manylinux_2_28_x86_64.whl
vecna_acs_engine-<version>-cp312-cp312-manylinux_2_28_aarch64.whl
vecna_acs_engine-<version>-cp312-cp312-macosx_11_0_arm64.whl
libacs_engine-linux-amd64.a
libacs_engine-linux-arm64.a
acs_engine.h
acs-src.tar.gz
SHA256SUMS
```

**Naming caveat — read before re-pinning to a different Python floor.** The
crate builds with `pyo3` feature `abi3-py311` (the upstream default), so
`maturin` natively tags the wheel `cp311-abi3` — one build is ABI-compatible
with CPython 3.11, 3.12, 3.13, ... The release workflow renames that file to
`cp312-cp312` to match the fixed asset-name contract above. This is safe only
because the consuming environment pins exactly CPython 3.12: `pip` treats
`cp312-cp312` as an exact-version tag, not an abi3 tag, so installing this
file under 3.11 or 3.13 will be rejected even though the compiled extension
would actually run fine there. If Vecna's executor image ever moves off
CPython 3.12 exactly, re-tag the renamed file to match (or drop the
relabeling and let downstream pin on the true `cp311-abi3` tag instead).

**Bumping upstream:** pick the new upstream `main` commit (or a real
`vX.Y.Z` tag once upstream cuts one), diff it against the current pin commit
above for `policy-engine/core/src/ffi.rs` — the C ABI signatures are the one
thing a Go binding hard-depends on, so a diff there means the Go side needs
matching changes, not just a recompile. Re-apply the five changes listed
above on the new commit (they're small and mechanical: one `crate-type`
entry, one new `cbindgen.toml`, three `pyproject.toml` fields, one directory
rename + 4 import lines, `__version__` bump), update the pin SHA and date in
this file, then tag `v<new-version>-vecna.1` and let the workflow publish. Nothing
in `agent-governance-golang/` was reused for the FFI recipe — despite the
directory's name it ships no cgo binding and no existing static-lib/cbindgen
setup to diff against; the `staticlib` + `cbindgen.toml` recipe above was
written from scratch against `ffi.rs` directly.
