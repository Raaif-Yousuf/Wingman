# ARM64 build (issue #367): design

Status: proposed

## Goal

Wingman runs natively on Snapdragon Copilot+ PCs. A release ships ARM64
artifacts next to the x64 ones, and CI keeps the ARM64 target compiling.

## Decisions proposed (the packaging spec does not cover any of these)

1. **Cross-compile on the x64 runner**, `--target aarch64-pc-windows-msvc`.
   Nothing is executed on ARM64 in CI, so the free x64 `windows-latest`
   runner is enough. Alternative: the `windows-11-arm` runner, which would
   also let the test suite run natively. Not chosen yet because the Rust
   toolchain pin and cache setup there are unmeasured.
2. **One package per architecture**, `ProcessorArchitecture="x64"` or
   `"arm64"` in the manifest (token `@ARCH@`), not `neutral`. A sparse
   package points at an external exe of one architecture, and the identity
   should say which. THEORY (unverified): an x64 package registers and runs
   under emulation on ARM64, which is why x64 stays the default.
3. **Asset names**: x64 keeps `wingman.exe` / `wingman.msix`. ARM64 adds
   `wingman-arm64.exe` / `wingman-arm64.msix`. Existing download links and
   `SHA256SUMS.txt` consumers do not change.
4. **install.ps1 follows the rustc host.** A local `cargo build --release`
   produces an exe for the rustc host, so the package architecture is read
   from `rustc -vV`. No cross-target local installs.
5. **Same package identity** (`RaaifYousuf.Wingman`) for both. A machine
   installs exactly one.

## Open for the owner

- Whether to also ship a universal installer that picks the architecture.
- Whether `cargo test` should run on a native ARM64 runner (decision 1
  alternative).
- Authenticode signing of the ARM64 exe uses the same certificate; no new
  secret.

## Not verified yet

Nothing here has run on ARM64 hardware. Win32 behaviour that could differ
(the low-level keyboard hook, Windows OCR, UIA) is expected to be identical
but is a THEORY (unverified) until someone with a Snapdragon PC runs it.
