# Reproducible builds and SBOM (issue #137)

## What is reproducible today

**`target\release\wingman.exe` built from a given commit is
bit-for-bit reproducible**, given:

- the pinned toolchain in [`rust-toolchain.toml`](../rust-toolchain.toml)
  (`1.98.1-x86_64-pc-windows-msvc`, matching what `rustc -V` reported on
  the machine that added the pin),
- `cargo build --release --locked` (`--locked` refuses to touch
  `Cargo.lock`, so a resolver change on the builder's machine cannot
  silently change which dependency versions get compiled in),
- `SOURCE_DATE_EPOCH` set to the release commit's timestamp
  (`git log -1 --format=%ct`) and `CARGO_INCREMENTAL=0`, and
- the `/Brepro` linker flag in [`.cargo/config.toml`](../.cargo/config.toml),
  which applies automatically to every `x86_64-pc-windows-msvc` build in
  this repository (local or CI) with no extra flag needed at the call site.

**What is not measured**: `wingman.msix` (packaged by
`packaging\Build-Msix.ps1` via `makeappx.exe`, then optionally signed) is
not covered by this doc. A self-signed or CA-issued Authenticode signature
embeds its own timestamp by design (that is what lets Windows validate the
signature after the certificate expires), so the `.msix` is expected to
differ byte-for-byte between two signed builds even when the `.exe` inside
it is identical; that is normal and not a reproducibility bug. Verifying
that the *unsigned* `.msix` (same `has_cert=false` path `release.yml` takes
when no signing secret is configured) is itself reproducible is unmeasured
and tracked as a follow-up, not claimed here.

## The measurement (MEASURED 2026-09-17)

Two clean copies of the same commit's source tree were placed in two
different absolute paths (`git archive HEAD | tar -x` into two scratch
directories, so nothing was shared between them but the git history), each
built with its own `CARGO_TARGET_DIR`, sequentially (never in parallel, per
this repo's shared-build-machine rule), with:

```
SOURCE_DATE_EPOCH=<git log -1 --format=%ct HEAD>
CARGO_INCREMENTAL=0
cargo build --release --locked
```

plus `--remap-path-prefix=<own source dir>=/wingman-src` so neither build's
absolute path leaked into the binary as a distinct string (both remap to
the same placeholder).

**Before `/Brepro`**: the two `wingman.exe` files were the same size
(1,888,256 bytes) but differed in exactly 20 bytes, in two small clusters:

- one 3-byte-wide difference at file offset ~257, inside the PE COFF
  header -- the `TimeDateStamp` field, which `link.exe` stamps with the
  wall-clock time of the link and which neither `SOURCE_DATE_EPOCH` nor
  `--remap-path-prefix` touches (both are rustc/source-level knobs; this
  field is written by the MSVC linker itself), and
- a 16-byte cluster around offset ~1,746,941, consistent with the CodeView
  debug directory's PDB GUID -- a value `link.exe` randomizes per link so
  it can be matched against the correct `.pdb` (which is unique to a
  specific link too), independent of the source or the requested epoch.

**With `/Brepro`** (`-C link-arg=/Brepro`, applied only to the final
`wingman` binary's link step -- it has no effect on how dependency crates
compile, so it does not by itself force a rebuild of the dependency
graph): the two `wingman.exe` files came out at 1,888,768 bytes each,
identical SHA-256 (`a7f89804e3cf6e070d4d306f6198add45e25ce7dfe0b697987e49fe3af393317`),
`cmp -l` reporting zero differing bytes. `/Brepro` is documented MSVC
linker behavior (derives the COFF timestamp and PDB GUID from the binary's
own content instead of the clock/RNG), which matches exactly the two
fields that differed without it.

### What this does not affect

`/Brepro` only changes what `link.exe` writes into the two fields above;
it changes nothing about what compiles or what the program does. Re-run
after adding it: `cargo test --bin wingman config::tests::` (78 passed, 0
failed) -- the same suite passed before the flag was added.

### Reproducing this measurement

```bash
export CARGO_TARGET_DIR=<your worktree's own target dir> RUSTC_WRAPPER=sccache CARGO_BUILD_JOBS=2
epoch=$(git log -1 --format=%ct HEAD)
git archive HEAD | (mkdir -p /tmp/repro-a && cd /tmp/repro-a && tar -x)
git archive HEAD | (mkdir -p /tmp/repro-b && cd /tmp/repro-b && tar -x)

cd /tmp/repro-a
SOURCE_DATE_EPOCH=$epoch CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/tmp/target-a \
  RUSTFLAGS="--remap-path-prefix=/tmp/repro-a=/wingman-src" \
  cargo build --release --locked

cd /tmp/repro-b
SOURCE_DATE_EPOCH=$epoch CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/tmp/target-b \
  RUSTFLAGS="--remap-path-prefix=/tmp/repro-b=/wingman-src" \
  cargo build --release --locked

sha256sum /tmp/target-a/release/wingman.exe /tmp/target-b/release/wingman.exe
```

(`.cargo/config.toml`'s `/Brepro` applies automatically to both builds
above since both source trees carry it. On Git Bash / MSYS, a leading `/`
in a linker flag gets path-translated unless it is embedded in a single
token like `-Clink-arg=/Brepro` or `MSYS_NO_PATHCONV=1` is set -- this
bit `.cargo/config.toml` itself, since TOML has no shell to mangle.)

## SBOM

`release.yml` generates a CycloneDX SBOM with `cargo cyclonedx` (installed
in the job via `cargo install cargo-cyclonedx --locked`; the tool itself is
Apache-2.0, permissive per CLAUDE.md rule 2 -- checked at
<https://crates.io/crates/cargo-cyclonedx> before adding it here) and
uploads it as a release asset alongside `wingman.exe`, `wingman.msix` and
`SHA256SUMS`. It describes `Cargo.lock`'s resolved dependency graph for
this build (name, version, license where crates.io metadata has it,
PURLs); it is not itself a reproducibility check; it is what
`THIRD_PARTY_NOTICES.md` already keeps by hand, in a machine-readable
format a downstream consumer's tooling can ingest directly.

## Workflow validation

`.github/workflows/release.yml` was checked with
[actionlint](https://github.com/rhysd/actionlint) (MIT-licensed; downloaded
to a scratch temp dir for this one-off check, not installed into the
repository or committed): no findings against the workflow syntax, shell
directives (`shell: pwsh` blocks parsed as PowerShell, not bash), or the
`${{ }}` expression contexts used.

## Done when (issue #137's own criterion)

"A release carries an SBOM and the build steps are reproducible from the
tag on a clean runner, or the doc states exactly which step is not yet."
This doc states it: `wingman.exe` is measured byte-for-byte reproducible
under the conditions above; `wingman.msix` is not measured (expected to
differ once signed, by design; the unsigned path is an open follow-up);
every release now carries a CycloneDX SBOM as a release asset.
