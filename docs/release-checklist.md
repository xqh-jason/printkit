# Release checklist

This file records the steps that cannot be enforced from the repository alone,
plus the release gates that must pass before a tag is published.

## GitHub repository settings (maintainer only)

These are not files and cannot be checked by CI:

- [ ] Description matches the release scope (macOS-first; Windows preview).
- [ ] Homepage points at the documentation entry point.
- [ ] Topics: `pdf`, `printing`, `rust`, `macos`, `windows`, `json`.
- [ ] Security → Private vulnerability reporting is enabled (see `SECURITY.md`).
- [ ] `main` requires the CI checks and at least one review before merge.
- [ ] Release tags are protected from force-push/deletion.
- [ ] Release assets are immutable after publication (if the repository setting
      is available).

## Before tagging

- [ ] `CHANGELOG.md` has a `## [<version>] - <date>` entry covering the release
      commit (the `Unreleased` section is merged into that heading).
- [ ] `Cargo.toml`, desktop package/Cargo/`tauri.conf.json`, SDK and i18n
      versions are equal (`scripts/release.sh` verifies this).
- [ ] The public protocol/schema snapshot is regenerated from the release
      commit. It must not still advertise removed methods such as `calib.*`,
      and its method, error and warning lists must match
      `schema/protocol-v1.json`. `public-snapshot-next` must be rebuilt from
      the release commit's tree: `git rev-parse public-snapshot-next^{tree}`
      and `git rev-parse <release-commit>^{tree}` must print the same id (one
      root commit, no `_work`/`dist`/`target`). The old `public-snapshot`
      branch is a frozen historical tip and is never the release tree.
- [ ] `THIRD-PARTY-NOTICES.md` was regenerated from the target-all SBOM
      (`cargo cyclonedx --target all` for the CLI and the desktop workspace,
      plus the production Node inventory and the first-party entries), so
      macOS, Linux and Windows runners produce the same component list. The
      generated notices and `assets/licenses/component-licenses.*` must have
      **zero** entries for `fabric`, `printkit-plugin-designer` or
      `printkit-plugin-templates`.
- [ ] `python3 scripts/gen-notices.py --check --sbom dist/sbom.cdx.json
      --licenses-dir assets/licenses` passes.
- [ ] `bash tests/scripts/s16_accept.sh --dist dist` passes under the default
      `core` preset; `plugins_core_only_e2e.sh` and `plugins_e2e.sh` also pass,
      covering the core-only and all-enabled modes.
- [ ] The third-party notice policy is still honest and best-effort: missing
      copyright lines are marked `[packaged metadata has no copyright line; see
      upstream source]`, Apache appendix `{yyyy} {name of copyright owner}`
      lines are not treated as attribution, and no release text claims full
      compliance.
- [ ] `bash scripts/gates.sh` passes.
- [ ] GitHub Actions `notices`, `workflow-lint` and the platform jobs are green.
- [ ] The tag is `v<version>` and the release notes link the changelog entry.
- [ ] `dist/` is build output and stays untracked; never commit it or move an
      existing tag to rebuild it. A rebuilt bundle needs a new patch version
      and a new immutable tag.

## Signing status

The current release pipeline produces ad-hoc signed macOS bundles and unsigned
Windows installers. Developer ID notarization and Windows Authenticode require
credentials and are documented as unverified in the README until they land.

## Artifact provenance

`publish` runs `actions/attest-build-provenance` for the archives and installers.
Consumers can verify the attestation with `gh attestation verify <artifact>
--repo xqh-jason/printkit`.

## Reproducibility

`BUILD-INFO` records the source commit, dirty marker, rustc version, build time,
`SOURCE_DATE_EPOCH` and `reproducible=not-byte-reproducible`. The build is not
claimed to be byte-for-byte reproducible: WebView/Tauri bundling and Rust
debuginfo still embed environment-dependent data. `SOURCE_DATE_EPOCH` is the
commit timestamp and is exported so tools that honor it can stabilize embedded
timestamps; two builds are not expected to have the same hash.
