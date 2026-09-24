# Third-party notices

Wingman is MIT-licensed. It links against the Rust crates below, all
permissively licensed with two flagged (non-blocking) exceptions (see
**Licensing note**). No source code has been copied from another project
into this repository yet; when that happens (AGENTS.md rule 2: MIT-only,
attributed here), the attribution goes in **Copied code**, below.

## How this list was generated

```powershell
cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc
```

filtered to the packages actually reachable from `copilot-ask`'s dependency
graph on the `x86_64-pc-windows-msvc` target (the platform this app ships
for), which excludes build-only tooling (`bindgen`, `clang-sys`) and other-OS
branches of shared dependencies (Wayland, X11, DRM, Objc2) that `xcap` pulls
in for Linux and macOS but that never compile into the Windows binary. This
mirrors what `cargo deny list` will report once `deny.toml` (plan §13, a
separate Phase 0 item) exists; regenerate this file whenever `Cargo.lock`
changes materially, by rerunning the command above against the current lock
file and updating the table.

142 crates resolve for this target as of `Cargo.lock` after issue #160
dropped the direct dependency `dirs` (and its `dirs-sys` -> `option-ext`
transitive chain, plus the `windows-sys` 0.61.2 pulled in only by
`dirs-sys`) in favor of a direct `SHGetKnownFolderPath` call through the
`windows` crate this project already depends on
(`src/known_folder.rs`).

## Direct dependencies

| crate | version | license | project |
|---|---|---|---|
| anyhow | 1.0.104 | MIT OR Apache-2.0 | https://github.com/dtolnay/anyhow |
| arboard | 3.6.1 | MIT OR Apache-2.0 | https://github.com/1Password/arboard |
| base64 | 0.23.1 | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 |
| embed-resource | 3.0.11 (build) | MIT | https://github.com/nabijaczleweli/rust-embed-resource |
| image | 0.25.10 | MIT OR Apache-2.0 | https://github.com/image-rs/image |
| serde | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| serde_json | 1.0.151 | MIT OR Apache-2.0 | https://github.com/serde-rs/json |
| toml | 1.1.6 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| ureq | 3.4.2 | MIT OR Apache-2.0 | https://github.com/algesten/ureq |
| windows | 0.62.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| xcap | 0.9.8 | **Apache-2.0 only** | https://github.com/nashaofu/xcap |

`xcap` is Apache-2.0 without an MIT alternative (AGENTS.md flags this: it is
permissive and allowed as a dependency, but code cannot be copied from it
under the MIT-only copied-code rule; only used as a library, never copied
from, here).

## Licensing note (flagged, not blocking)

Issue #160 removed the one non-permissive exception this file used to carry
here: `option-ext` 0.2.0 (MPL-2.0, weak copyleft), pulled in transitively by
the direct dependency `dirs` (`dirs` -> `dirs-sys` -> `option-ext`), used
only to resolve `%APPDATA%`. `Config::path()`/`Config::old_path()` now call
`SHGetKnownFolderPath(FOLDERID_RoamingAppData)` directly through the
`windows` crate (`src/known_folder.rs`), so `dirs`, `dirs-sys` and
`option-ext` are no longer in the dependency graph at all, and `deny.toml`'s
matching per-crate exception has been removed alongside them (`cargo tree -i
option-ext` finds nothing).

Two crates still use a permissive license named individually in
`deny.toml`'s per-crate exceptions rather than folded into the allowlist
(MIT, Apache-2.0, BSD-2/3, ISC, Zlib, Unicode-3.0, Unlicense):

- **`clipboard-win` 5.4.1** and **`error-code` 3.4.0**, both BSL-1.0 (the
  Boost Software License, a short, OSI-approved permissive license, not to
  be confused with the Business Source License that also abbreviates BSL).
- **`webpki-roots` 1.0.9**, CDLA-Permissive-2.0 (Community Data License
  Agreement, Permissive, a permissive license for the Mozilla root
  certificate data it bundles, used transitively through `rustls` /
  `ureq`).

Both are genuinely permissive; `deny.toml`'s narrow per-crate exceptions for
them (rather than widening the allowlist) are the accepted, non-blocking
state, not a pending action.

## All dependencies (this platform)

| crate | version | license |
|---|---|---|
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| anyhow | 1.0.104 | MIT OR Apache-2.0 |
| arboard | 3.6.1 | MIT OR Apache-2.0 |
| autocfg | 1.5.1 | Apache-2.0 OR MIT |
| base64 | 0.23.1 | MIT OR Apache-2.0 |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT |
| bytemuck_derive | 1.12.1 | Zlib OR Apache-2.0 OR MIT |
| byteorder-lite | 0.1.0 | Unlicense OR MIT |
| bytes | 1.12.1 | MIT |
| cc | 1.4.6 | MIT OR Apache-2.0 |
| cfg-if | 1.0.4 | MIT OR Apache-2.0 |
| clipboard-win | 5.4.1 | BSL-1.0 |
| cookie | 0.18.2 | MIT OR Apache-2.0 |
| cookie_store | 0.22.1 | MIT OR Apache-2.0 |
| crc32fast | 1.5.2 | MIT OR Apache-2.0 |
| deranged | 0.5.8 | MIT OR Apache-2.0 |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 |
| document-features | 0.2.12 | MIT OR Apache-2.0 |
| embed-resource | 3.0.11 | MIT |
| equivalent | 1.0.2 | Apache-2.0 OR MIT |
| error-code | 3.4.0 | BSL-1.0 |
| fax | 0.2.7 | MIT |
| fdeflate | 0.3.7 | MIT OR Apache-2.0 |
| find-msvc-tools | 0.1.12 | MIT OR Apache-2.0 |
| flate2 | 1.1.10 | MIT OR Apache-2.0 |
| form_urlencoded | 1.2.2 | MIT OR Apache-2.0 |
| getrandom | 0.2.17 | MIT OR Apache-2.0 |
| half | 2.7.1 | MIT OR Apache-2.0 |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 |
| http | 1.5.0 | MIT OR Apache-2.0 |
| httparse | 1.10.1 | MIT OR Apache-2.0 |
| icu_collections | 2.3.0 | Unicode-3.0 |
| icu_locale_core | 2.3.0 | Unicode-3.0 |
| icu_normalizer | 2.3.0 | Unicode-3.0 |
| icu_normalizer_data | 2.3.0 | Unicode-3.0 |
| icu_properties | 2.3.0 | Unicode-3.0 |
| icu_properties_data | 2.3.0 | Unicode-3.0 |
| icu_provider | 2.3.1 | Unicode-3.0 |
| idna | 1.1.0 | MIT OR Apache-2.0 |
| idna_adapter | 1.2.2 | Apache-2.0 OR MIT |
| image | 0.25.10 | MIT OR Apache-2.0 |
| indexmap | 2.14.2 | Apache-2.0 OR MIT |
| itoa | 1.0.18 | MIT OR Apache-2.0 |
| libc | 0.2.189 | MIT OR Apache-2.0 |
| litemap | 0.8.3 | Unicode-3.0 |
| litrs | 1.0.0 | MIT OR Apache-2.0 |
| log | 0.4.34 | MIT OR Apache-2.0 |
| memchr | 2.8.3 | Unlicense OR MIT |
| miniz_oxide | 0.8.9 | MIT OR Zlib OR Apache-2.0 |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 |
| moxcms | 0.8.1 | BSD-3-Clause OR Apache-2.0 |
| num-conv | 0.2.2 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| percent-encoding | 2.3.2 | MIT OR Apache-2.0 |
| png | 0.18.1 | MIT OR Apache-2.0 |
| potential_utf | 0.1.6 | Unicode-3.0 |
| powerfmt | 0.2.0 | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| pxfm | 0.1.30 | BSD-3-Clause OR Apache-2.0 |
| quick-error | 2.0.1 | MIT/Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| ring | 0.17.14 | Apache-2.0 AND ISC |
| rustc_version | 0.4.1 | MIT OR Apache-2.0 |
| rustls | 0.23.45 | Apache-2.0 OR ISC OR MIT |
| rustls-pki-types | 1.15.1 | MIT OR Apache-2.0 |
| rustls-webpki | 0.103.15 | ISC |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 |
| semver | 1.0.28 | MIT OR Apache-2.0 |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.151 | MIT OR Apache-2.0 |
| serde_spanned | 1.1.1 | MIT OR Apache-2.0 |
| shlex | 2.0.1 | MIT OR Apache-2.0 |
| simd-adler32 | 0.3.10 | MIT |
| smallvec | 1.16.1 | MIT OR Apache-2.0 |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 |
| subtle | 2.6.1 | BSD-3-Clause |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.5 | MIT OR Apache-2.0 |
| synstructure | 0.13.2 | MIT |
| thiserror | 2.0.20 | MIT OR Apache-2.0 |
| thiserror-impl | 2.0.20 | MIT OR Apache-2.0 |
| tiff | 0.11.3 | MIT |
| time | 0.3.55 | MIT OR Apache-2.0 |
| time-core | 0.1.9 | MIT OR Apache-2.0 |
| time-macros | 0.2.32 | MIT OR Apache-2.0 |
| tinystr | 0.8.4 | Unicode-3.0 |
| toml | 1.1.6+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_datetime | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_parser | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_writer | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| untrusted | 0.9.0 | ISC |
| ureq | 3.4.2 | MIT OR Apache-2.0 |
| ureq-proto | 0.6.3 | MIT OR Apache-2.0 |
| url | 2.5.8 | MIT OR Apache-2.0 |
| utf8-zero | 0.8.1 | MIT OR Apache-2.0 |
| utf8_iter | 1.0.4 | Apache-2.0 OR MIT |
| version_check | 0.9.5 | MIT/Apache-2.0 |
| vswhom | 0.1.0 | MIT |
| vswhom-sys | 0.1.3 | MIT |
| webpki-roots | 1.0.9 | CDLA-Permissive-2.0 |
| weezl | 0.1.12 | MIT OR Apache-2.0 |
| widestring | 1.2.1 | MIT OR Apache-2.0 |
| windows | 0.62.2 | MIT OR Apache-2.0 |
| windows-collections | 0.3.2 | MIT OR Apache-2.0 |
| windows-core | 0.62.2 | MIT OR Apache-2.0 |
| windows-future | 0.3.2 | MIT OR Apache-2.0 |
| windows-implement | 0.60.2 | MIT OR Apache-2.0 |
| windows-interface | 0.59.3 | MIT OR Apache-2.0 |
| windows-link | 0.2.1 | MIT OR Apache-2.0 |
| windows-numerics | 0.3.1 | MIT OR Apache-2.0 |
| windows-result | 0.4.1 | MIT OR Apache-2.0 |
| windows-strings | 0.5.1 | MIT OR Apache-2.0 |
| windows-sys | 0.59.0 | MIT OR Apache-2.0 |
| windows-sys | 0.60.2 | MIT OR Apache-2.0 |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 |
| windows-targets | 0.53.5 | MIT OR Apache-2.0 |
| windows-threading | 0.2.1 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.53.1 | MIT OR Apache-2.0 |
| winnow | 1.0.4 | MIT |
| winreg | 0.55.0 | MIT |
| writeable | 0.6.4 | Unicode-3.0 |
| xcap | 0.9.8 | Apache-2.0 |
| yoke | 0.8.3 | Unicode-3.0 |
| yoke-derive | 0.8.2 | Unicode-3.0 |
| zerocopy | 0.8.57 | BSD-2-Clause OR Apache-2.0 OR MIT |
| zerocopy-derive | 0.8.57 | BSD-2-Clause OR Apache-2.0 OR MIT |
| zerofrom | 0.1.8 | Unicode-3.0 |
| zerofrom-derive | 0.1.7 | Unicode-3.0 |
| zeroize | 1.9.0 | Apache-2.0 OR MIT |
| zerotrie | 0.2.5 | Unicode-3.0 |
| zerovec | 0.11.8 | Unicode-3.0 |
| zerovec-derive | 0.11.6 | Unicode-3.0 |
| zlib-rs | 0.6.7 | Zlib |
| zmij | 1.0.23 | MIT |
| zune-core | 0.5.3 | MIT OR Apache-2.0 OR Zlib |
| zune-jpeg | 0.5.15 | MIT OR Apache-2.0 OR Zlib |

Full license texts are published by each project at the repository URLs
above (direct dependencies) or on crates.io (`https://crates.io/crates/<name>`
for any transitive one) and are not reproduced here; none of them require
that, only attribution, which this file provides.

## Copied code

None yet. When a snippet is copied from another MIT-licensed project (per
AGENTS.md rule 2, e.g. the planned study of PowerToys' Text Extractor overlay
or Flow Launcher's fuzzy ranking, see the expansion plan §14), it is listed
here with the source file, the origin project, and its license text.
