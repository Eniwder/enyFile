# Text fallback backport

This directory contains the published `fontique 0.11.1` and `parley 0.11.1`
sources with a backport of [Parley PR #689](https://github.com/linebender/parley/pull/689).
Slint remains at **1.18.1**. The application does not implement its own font
selector or renderer and does not register a hard-coded fallback family.

## Provenance

- Upstream repository: <https://github.com/linebender/parley>
- PR head examined: `8f1958db224ea43294c82c3868176ad0388d388e`
- Reviewed/adopted locally: 2026-11-08. The PR was open/unmerged at that time.
- Base: crates.io `fontique 0.11.1`, checksum
  `6688bc1294fe7117d788937b6c53480169b29c566954af490830d4c09da9516a`.
- Base: crates.io `parley 0.11.1`, checksum
  `22d2ff88bd3f7d68d1d9b09c7e6209f9a8e8c05088295140a2bcf2e9b17038c5`.
- Reproducible fallback source delta, excluding the capacity adjustment and
  API compatibility cleanups described below:
  [`patches/pr689-0.11.1.diff`](patches/pr689-0.11.1.diff).
- Both packages retain their upstream `LICENSE-MIT` and `LICENSE-APACHE`.
  The MIT option and attribution are included in `THIRD-PARTY-NOTICES.txt`,
  copied into release packages. No Windows font files are redistributed.

## Backport and local delta

The existing script/locale candidates are the fast path. When those cannot
cover the actual shaping cluster, Parley supplies the characters to Fontique,
which asks the OS for a suitable fallback. Results, including negative results,
are cached. Windows uses DirectWrite `MapCharacters()`.

The PR does not apply verbatim to the published 0.11.1 sources. The backport
retains 0.11.1's font-loading implementation, adapts the `CharCluster.chars`
field and uses edition-2021-compatible syntax in the Windows backend. The PR's
unrelated empty-layout cursor-metrics change is not included.

The only behavioral delta from the tested fallback backport is
`TEXT_FALLBACK_CACHE_MAX_ENTRIES = 8192` in
`fontique/src/collection/mod.rs` (the PR used 1024). The cache remains bounded
and retains the PR's existing eviction/invalidation strategy. This avoids
repeated OS calls for large multilingual directory working sets without
introducing a new cache algorithm. A working set larger than the cap can still
evict entries. No research counters, profiling switches, font bundles or
additional production dependencies are included.

This product targets Windows x64. Other platform backends follow the PR's
type changes but have not been built or tested as part of this backport.

### API compatibility cleanups

- Fontique's 161 fixed four-byte script tags use `Script::from_bytes(*b"....")`
  instead of the deprecated `from_str_unchecked`. In the locked Parlance 0.1.1,
  the old constructor delegates to the same byte constructor; tag values and
  ordering are unchanged.
- Parley's `bidi::mask` retains `BidiClass::to_icu4c_value` with a function-local
  `allow(deprecated)`. ICU4X 2.3.0 deprecated this conversion without a const
  replacement. This preserves the upstream bidirectional algorithm and avoids
  relying on the private representation or maintaining a second class mapping.
  Other deprecation warnings remain enabled. Revisit this allowance when
  migrating Parley/ICU4X.

## Verification

See [`../../docs/font-fallback.md`](../../docs/font-fallback.md) from the
repository root (`docs/font-fallback.md`) for results and measurement limits.

```powershell
cargo test --release --locked --manifest-path app/Cargo.toml
cargo tree --locked --manifest-path app/Cargo.toml -i fontique
cargo tree --locked --manifest-path app/Cargo.toml -i parley
```

`theme::tests::os_supported_characters_survive_normal_font_selection` checks
the real selected-font/layout path against the characters the current OS can
provide. It tolerates absent optional language fonts and does not assume a
particular Chinese font exists. Rendering, paging, and performance also need
interactive verification; unit tests alone do not prove those properties.
Some upstream crate-local tests use the upstream development repository's font
fixtures, which are not distributed here.

## Migration to an upstream fix

1. Check that a **released** Parley/Fontique pair contains actual-character OS
   fallback, and that the intended Slint version supports that pair. Merely
   upgrading to 0.12.0 did not contain this fix at review time.
2. Remove both `[patch.crates-io]` entries from `app/Cargo.toml`; update Slint,
   the dev-only Parley dependency and `Cargo.lock` together as needed.
3. Run the headless regression, application tests and multilingual preview.
   Compare Japanese/ASCII screen pixels and widths, and repeat the 5,000-name
   paging/scrolling measurements. Include a workload exceeding 1,024 distinct
   fallback characters to evaluate the upstream cache policy.
4. Do not restore the former `SLINT_FONT_PATH` workaround. Keep OS-selected
   fallback and verify behavior when optional fonts are absent.
5. Once the registry build passes those checks, remove the two vendored
   packages and patch file and update this note and the packaged notices.
