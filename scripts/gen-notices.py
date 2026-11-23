#!/usr/bin/env python3
"""Generate THIRD-PARTY-NOTICES.md and the per-component license appendix.

The SBOM is the reproducible inventory. Canonical license texts come from the
SPDX license-list-data release recorded in assets/licenses/SOURCES.md. Per-
component copyright lines and full license texts come from
assets/licenses/component-licenses.json, which is rebuilt with Mozilla
cargo-about for the Rust dependency graph and with pnpm's license inventory
plus the installed package files for the Node dependency graph.

Usage:
    gen-notices.py [--sbom dist/sbom.cdx.json] [--output THIRD-PARTY-NOTICES.md]
    gen-notices.py --check ...
    gen-notices.py --check-cargo ...
    gen-notices.py --rebuild-legal-data --about-cli ... --about-desktop ... \
        --pnpm-json ...

The --check modes never need cargo-about, pnpm or the package caches: they
compare the committed Markdown against data committed under assets/licenses.
"""

import argparse
import hashlib
import json
import os
import re
import sys
from pathlib import Path

LICENSE_TOKEN = re.compile(r"[A-Za-z0-9][A-Za-z0-9.+-]*")
LICENSE_WORDS = {"AND", "OR", "WITH", "EXPRESSION", "LICENSE", "ID", "NAME"}
LICENSE_DIR = Path("assets/licenses")
LICENSE_URL = "https://spdx.org/licenses/{}.html"
LICENSES_NAME = "assets/licenses/component-licenses.txt"
LEGAL_DATA_NAME = "component-licenses.json"
REPOSITORY = "https://github.com/xqh-jason/printkit"
SOURCES_NAME = "SOURCES.md"

# A license file is any top-level package file with one of these names. NOTICE
# files are collected separately so their Apache-style propagation is visible.
LICENSE_FILE = re.compile(
    r"^(license|licence|copying|copyright|unlicense)([._-].*)?$", re.I
)
NOTICE_FILE = re.compile(r"^notices?([._-].*)?$", re.I)
# A notice starts with a capitalised "Copyright", a copyright sign or "(C)".
COPYRIGHT_LINE = re.compile(r"^(?:Copyright|COPYRIGHT|©)\s|^\(C\)\s")
COPYRIGHT_PLACEHOLDER = re.compile(
    r"(<year>|<copyright holders>|\[yyyy\]|\[name of copyright owner\]"
    r"|\{yyyy\}|\{name of copyright owner\}|\{year\}|\{owner\}"
    r"|<name of author>|<owner>|copyright holders)",
    re.I,
)
MAX_COPYRIGHT_LINES = 8
NO_COPYRIGHT_MARKER = (
    "[packaged metadata has no copyright line; see upstream source]"
)
NO_LICENSE_MARKER = (
    "[packaged metadata has no license text; see upstream source]"
)
TEXT_FENCE = re.compile(r"`+")

SOURCE_ROW = re.compile(
    r"^\| ([^|]+) \| `([^`]+)` \| <([^>]+)> \| `([0-9a-f]{64})` \|$"
)

ROOT = Path(__file__).resolve().parents[1]


def normalized_text(value):
    return value.replace("\r\n", "\n").replace("\r", "\n").strip("\n") + "\n"


def digest(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def field(value):
    return " ".join(str(value).split()).replace("`", r"\`")


def license_expression(component):
    values = []
    for entry in component.get("licenses", []):
        value = entry.get("expression")
        if not value:
            value = entry.get("license", {}).get("id")
        if not value:
            value = entry.get("license", {}).get("name")
        if value:
            values.append(value)
    return " OR ".join(values) if values else "Unknown"


def license_ids(expression):
    ids = set()
    for token in LICENSE_TOKEN.findall(expression):
        if token.upper() not in LICENSE_WORDS:
            ids.add(token)
    return ids


def source_url(component):
    references = component.get("externalReferences", [])
    for kind in ("vcs", "website", "distribution"):
        for reference in references:
            if reference.get("type") == kind and reference.get("url"):
                return reference["url"]

    if first_party(component):
        return REPOSITORY
    purl = component.get("purl", "")
    name = component["name"]
    if purl.startswith("pkg:npm/"):
        return "https://www.npmjs.com/package/" + name
    if purl.startswith("pkg:cargo/"):
        return "https://crates.io/crates/{}/{}".format(
            name, component.get("version", "")
        )
    return ""


def first_party(component):
    """The CLI binary and the workspace crates it is built from.

    `printkit` alone is the application component emitted by cargo-cyclonedx;
    the workspace crates use the `printkit-` prefix and the npm packages use
    the `@printkit/` scope. A similarly named third-party package must not be
    absorbed by a bare `startswith("printkit")` check.
    """
    name = component["name"]
    return (
        name == "printkit"
        or name.startswith("printkit-")
        or name.startswith("@printkit/")
    )


def sort_key(component):
    return (
        component["name"].casefold(),
        component.get("version", ""),
        component.get("purl", ""),
    )


def workspace_versions():
    """Name -> version for the workspace's own npm packages."""
    versions = {}
    for pattern in ("packages/*/package.json", "apps/*/package.json"):
        for manifest in sorted(ROOT.glob(pattern)):
            try:
                data = json.loads(manifest.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            name, version = data.get("name"), data.get("version")
            if name and version:
                versions[name] = version
    return versions


def component_version(component, versions):
    version = component.get("version")
    if version:
        return version
    return versions.get(component["name"], "unknown")


def component_line(component, versions):
    name = field(component["name"])
    version = field(component_version(component, versions))
    license = field(license_expression(component))
    author = field(component.get("author") or "")
    source = source_url(component)
    fields = ["- **{}** `{}` — {}".format(name, version, license)]
    if author:
        fields.append("author: {}".format(author))
    if source:
        fields.append("<{}>".format(source))
    else:
        fields.append("source: not recorded in the SBOM")
    return " — ".join(fields)


def fs_fence(text):
    longest = max((len(match.group(0)) for match in TEXT_FENCE.finditer(text)), default=0)
    ticks = "`" * max(3, longest + 1)
    return "{0}\n{1}{0}".format(ticks, text if text.endswith("\n") else text + "\n")


# ---------------------------------------------------------------------------
# Canonical license texts and their provenance


def load_sources(licenses_dir):
    """Return {id: (file, url, sha256)} after verifying every text file."""
    path = licenses_dir / SOURCES_NAME
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise SystemExit("cannot read {}: {}".format(path, error))
    rows = {}
    for line in lines:
        match = SOURCE_ROW.match(line)
        if not match:
            continue
        license_id, file_name, url, expected = match.groups()
        rows[license_id] = (file_name, url, expected)
    if not rows:
        raise SystemExit("{} has no license source rows".format(path))
    indexed = {file_name for file_name, _, _ in rows.values()}
    auxiliary = {"component-licenses.txt"}
    for text_file in sorted(licenses_dir.glob("*.txt")):
        if text_file.name in auxiliary:
            continue
        if text_file.name not in indexed:
            raise SystemExit(
                "{} is not indexed in {}".format(text_file.name, path)
            )
    for license_id, (file_name, _, expected) in rows.items():
        text_file = licenses_dir / file_name
        if not text_file.is_file():
            raise SystemExit("missing canonical text {}".format(text_file))
        actual = digest(normalized_text(text_file.read_text(encoding="utf-8")))
        if actual != expected:
            raise SystemExit(
                "{} does not match the SHA-256 in {} (expected {}, got {})".format(
                    text_file, path, expected, actual
                )
            )
    return rows


# ---------------------------------------------------------------------------
# Authoritative per-component data


def cargo_registry_roots():
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    root = cargo_home / "registry" / "src"
    if not root.is_dir():
        return []
    return sorted(path for path in root.iterdir() if path.is_dir())


def read_json(path, label):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit("cannot read {} {}: {}".format(label, path, error))


def about_crates(document, texts):
    """(name, version) -> list of license text references from cargo-about."""
    crates = {}
    for license_entry in document.get("licenses", []):
        text = normalized_text(license_entry.get("text") or "")
        if not text.strip():
            continue
        sha = digest(text)
        texts[sha] = text
        license_id = license_entry.get("id") or "custom"
        for used in license_entry.get("used_by", []):
            crate = used.get("crate")
            if not crate:
                continue
            key = (crate["name"], crate["version"])
            source_path = license_entry.get("source_path") or ""
            source = "cargo:{}@{}/{}".format(
                crate["name"],
                crate["version"],
                Path(source_path).name or "LICENSE",
            )
            reference = {"id": license_id, "source": source, "sha256": sha}
            crates.setdefault(key, []).append(reference)
    return crates


def pnpm_packages(document):
    """(name, version) -> inventory entry from pnpm licenses --long --json."""
    packages = {}
    for license_id, entries in document.items():
        for entry in entries:
            for version in entry.get("versions", []):
                packages[(entry["name"], version)] = {
                    "license": entry.get("license") or license_id,
                    "author": entry.get("author") or "",
                    "paths": entry.get("paths") or [],
                }
    return packages


def canonical_reference(license_id, sources, texts, licenses_dir):
    if license_id not in sources:
        return None
    file_name, _, _ = sources[license_id]
    text = normalized_text((licenses_dir / file_name).read_text(encoding="utf-8"))
    sha = digest(text)
    texts[sha] = text
    return {
        "id": license_id,
        "source": "SPDX:{} (assets/licenses/{})".format(license_id, file_name),
        "sha256": sha,
    }


def guess_license_id(file_name, text, ids):
    if len(ids) == 1:
        return next(iter(ids))
    stem = file_name.casefold()
    for license_id in sorted(ids):
        head = license_id.casefold().split("-")[0]
        if head and head in stem:
            return license_id
    match = re.search(r"SPDX-License-Identifier:\s*([A-Za-z0-9.+-]+)", text)
    if match and match.group(1) in ids:
        return match.group(1)
    return "custom"


def package_directory(component, pnpm_entry):
    name = component["name"]
    version = component.get("version", "")
    purl = component.get("purl", "")
    if purl.startswith("pkg:cargo/"):
        for registry in cargo_registry_roots():
            candidate = registry / "{}-{}".format(name, version)
            if candidate.is_dir():
                return candidate
        return None
    if purl.startswith("pkg:npm/"):
        if pnpm_entry:
            for path in pnpm_entry.get("paths", []):
                candidate = Path(path)
                if candidate.is_dir():
                    return candidate
        return None
    return None


def package_files(directory):
    licenses, notices = [], []
    if directory is None:
        return licenses, notices
    for path in sorted(Path(directory).iterdir()):
        if not path.is_file():
            continue
        if NOTICE_FILE.match(path.name):
            notices.append(path)
        elif LICENSE_FILE.match(path.name):
            licenses.append(path)
    return licenses, notices


def local_references(paths, prefix, ids, texts, notice=False):
    references = []
    for path in paths:
        text = normalized_text(path.read_text(encoding="utf-8", errors="replace"))
        sha = digest(text)
        texts[sha] = text
        license_id = "NOTICE" if notice else guess_license_id(path.name, text, ids)
        references.append(
            {
                "id": license_id,
                "source": "{}/{}".format(prefix, path.name),
                "sha256": sha,
            }
        )
    return references


def copyright_lines(references, texts):
    lines = []
    for reference in references:
        text = texts.get(reference["sha256"], "")
        for raw in text.splitlines():
            line = " ".join(raw.split())
            # License files sometimes put the notice in a heading or a
            # comment, e.g. `# Copyright (c) 2015 Calvin Metcalf` or
            # `// Copyright 2015-2016 Brian Smith.`
            line = re.sub(r"^(?:#+|>+|[-*]|//|<!--|;)\s+", "", line)
            line = re.sub(r"\s*-->\s*$", "", line)
            if not COPYRIGHT_LINE.match(line):
                continue
            if COPYRIGHT_PLACEHOLDER.search(line):
                continue
            if line not in lines:
                lines.append(line)
    return lines[:MAX_COPYRIGHT_LINES]


def merge_references(*groups):
    merged = {}
    for group in groups:
        for reference in group:
            merged.setdefault(reference["sha256"], reference)
    return sorted(merged.values(), key=lambda item: (item["id"], item["source"]))


def build_component(component, about, pnpm, sources, licenses_dir, texts):
    purl = component.get("purl", "")
    name = component["name"]
    version = component.get("version") or ""
    expression = license_expression(component)
    ids = license_ids(expression)
    entry = {
        "purl": purl,
        "name": name,
        "version": version,
        "license": expression,
        "author": field(component.get("author") or ""),
        "texts": [],
        "notices": [],
        "copyright": [],
    }
    package = pnpm.get((name, version)) if purl.startswith("pkg:npm/") else None
    if package and package.get("author"):
        entry["author"] = field(package["author"])
    directory = package_directory(component, package)
    files, notice_files = package_files(directory)

    references = []
    if purl.startswith("pkg:cargo/"):
        if files:
            # cargo-about validates the dependency graph and license
            # expression, but the registry package carries every license file
            # for that exact version (including MIT when the expression is
            # `MIT OR Apache-2.0`). Reproduce all of them.
            references = local_references(
                files, "cargo:{}@{}".format(name, version), ids, texts
            )
        elif about.get((name, version)):
            references = about[(name, version)]
    elif purl.startswith("pkg:npm/"):
        prefix = "npm:{}@{}".format(name, version)
        if files:
            references = local_references(files, prefix, ids, texts)
        if not references:
            for license_id in sorted(ids):
                reference = canonical_reference(
                    license_id, sources, texts, licenses_dir
                )
                if reference is not None:
                    references.append(reference)

    if purl.startswith("pkg:cargo/") and not references:
        for license_id in sorted(ids):
            reference = canonical_reference(license_id, sources, texts, licenses_dir)
            if reference is not None:
                references.append(reference)

    entry["texts"] = merge_references(references)
    if notice_files:
        prefix = (
            "cargo:{}@{}".format(name, version)
            if purl.startswith("pkg:cargo/")
            else "npm:{}@{}".format(name, version)
        )
        entry["notices"] = local_references(
            notice_files, prefix, ids, texts, notice=True
        )
    entry["copyright"] = copyright_lines(entry["texts"], texts)
    return entry


def rebuild_legal_data(args):
    sbom = read_json(args.sbom, "SBOM")
    texts = {}
    about = about_crates(read_json(args.about_cli, "cargo-about CLI"), texts)
    for key, value in about_crates(
        read_json(args.about_desktop, "cargo-about desktop"), texts
    ).items():
        about.setdefault(key, value)
    pnpm = pnpm_packages(read_json(args.pnpm_json, "pnpm licenses"))

    licenses_dir = Path(args.licenses_dir)
    sources = load_sources(licenses_dir)
    components = [
        build_component(c, about, pnpm, sources, licenses_dir, texts)
        for c in sbom.get("components") or []
        if not first_party(c)
    ]
    components.sort(key=lambda c: (c["name"].casefold(), c["version"], c["purl"]))
    data = {
        "format": 1,
        "generated_with": (
            "cargo-about {}; pnpm licenses list --long; SPDX license-list-data "
            "v3.29.0".format(args.about_version)
        ),
        "components": components,
        "texts": {sha: texts[sha] for sha in sorted(texts)},
    }
    with_copyright = sum(1 for c in components if c["copyright"])
    notices = sum(len(c["notices"]) for c in components)
    path = licenses_dir / LEGAL_DATA_NAME
    path.write_text(
        json.dumps(data, indent=1, ensure_ascii=False, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    missing = [c["purl"] for c in components if not (c["texts"] or c["notices"])]
    print(
        "legal data: {} components, {} with copyright lines, {} without, "
        "{} license texts, {} NOTICE files{}".format(
            len(components),
            with_copyright,
            len(components) - with_copyright,
            sum(len(c["texts"]) for c in components),
            notices,
            "" if not missing else ", {} without any license text".format(len(missing)),
        )
    )


# ---------------------------------------------------------------------------
# Rendering


def legal_entries(components, data, licenses_dir):
    by_purl = {entry["purl"]: entry for entry in data["components"]}
    texts = data["texts"]
    entries = []
    missing = []
    for component in components:
        if first_party(component):
            continue
        purl = component.get("purl", "")
        entry = by_purl.get(purl)
        if entry is None:
            missing.append(purl)
            entry = {
                "purl": purl,
                "name": component["name"],
                "version": component.get("version", ""),
                "license": license_expression(component),
                "author": field(component.get("author") or ""),
                "texts": [],
                "notices": [],
                "copyright": [],
            }
        entries.append((component, entry, texts))
    if missing:
        raise SystemExit(
            "{} is stale for {} SBOM component(s); first: {}".format(
                licenses_dir / LEGAL_DATA_NAME, len(missing), missing[0]
            )
        )
    return entries


def render_notices(components, data, licenses_dir, sources):
    versions = workspace_versions()
    entries = legal_entries(components, data, licenses_dir)
    third_party = [c for c in components if not first_party(c)]
    own = [c for c in components if first_party(c)]
    mpl = sorted(
        {
            c["name"]
            for c in third_party
            if "MPL-2.0" in license_expression(c)
        }
    )
    used = set()
    for component in components:
        used.update(license_ids(license_expression(component)))
    used.add("OFL-1.1")
    missing_texts = sorted(used - set(sources), key=str.casefold)
    if missing_texts:
        raise SystemExit(
            "missing canonical license text for: " + ", ".join(missing_texts)
        )

    lines = [
        "# Third-party notices",
        "",
        "Generated from the CycloneDX SBOM by `scripts/gen-notices.py`; do not",
        "edit this file by hand. Regenerate the legal inputs and the Markdown:",
        "",
        "```sh",
        "bash scripts/gen-legal-notices.sh",
        "python3 scripts/gen-notices.py --check --sbom dist/sbom.cdx.json",
        "```",
        "",
        (
            "The SBOM contains **{} components**: **{} third-party** and **{}** "
            "first-party project entries. The first-party entries are listed "
            "separately for traceability and carry the project's own license."
        ).format(len(components), len(third_party), len(own)),
        "",

        "Copyright in each third-party component remains with its upstream",
        "authors. These notices aggregate the license texts and source links",
        "shipped by those packages; they do not reassign copyright or claim",
        "that the inventory is complete. This is a best-effort disclosure, not",
        "a statement of full compliance.",
        "",
        "## NOTICES policy",
        "",
        "- Each inventory entry names the component, its version, its SPDX",
        "  license expression, the author string from package metadata, and a",
        "  source link when the SBOM has one.",
        "- Rust license expressions are validated with Mozilla `cargo-about`",
        "  ({}); the text is reproduced from every LICENSE file of the exact".format(
            data.get("generated_with", "").split(";")[0]
        ),
        "  registry package, including both alternatives of a dual license.",
        "  Node data combines `pnpm licenses list --long --json` with the",
        "  LICENSE files installed from the npm package tarballs. The result is",
        "  committed in `assets/licenses/component-licenses.json`; the full",
        "  texts are in `assets/licenses/component-licenses.txt`.",
        "- A component whose packaged license text contains no copyright line",
        "  is marked `{}`. The marker describes the package metadata received;".format(
            NO_COPYRIGHT_MARKER
        ),
        "  it is not a claim that the component has no copyright holder.",
        "- Apache-2.0 license files include an appendix template line such as",
        "  `Copyright {yyyy} {name of copyright owner}` (some releases use",
        "  square brackets instead of braces). It is license boilerplate, not a",
        "  component attribution, and is never copied into the copyright field.",
        "- The license texts are the terms, not a per-component copyright",
        "  notice. The upstream author/package metadata is not presented as a",
        "  copyright statement.",
        "- Canonical license texts are verbatim SPDX `license-list-data`",
        "  v3.29.0 files. `assets/licenses/SOURCES.md` records the URL and",
        "  SHA-256 of every text; `--check` verifies them before passing.",
        "  SPDX MIT/BSD/ISC texts are templates whose holder fields are filled",
        "  by the per-component notices rather than by a project-wide holder.",
        "- `@printkit/sdk` is first-party. It has no runtime npm dependencies;",
        "  its TypeScript/esbuild tooling is development-only and is not shipped.",
        "",
        "## Third-party components",
        "",
    ]
    lines.extend(component_line(c, versions) for c in sorted(third_party, key=sort_key))
    lines.extend(["", "## First-party components", ""])
    lines.extend(component_line(c, versions) for c in sorted(own, key=sort_key))
    lines.extend(["", "## Per-component copyright notices", ""])
    for component, entry, _ in sorted(
        ((c, e, t) for c, e, t in entries), key=lambda item: sort_key(item[0])
    ):
        copyright_lines_text = entry["copyright"]
        prefix = "- **{}** `{}` — ".format(field(entry["name"]), field(entry["version"]))
        if copyright_lines_text:
            suffix = " / ".join(copyright_lines_text)
        else:
            suffix = NO_COPYRIGHT_MARKER
        lines.append(prefix + suffix)
        if not entry["texts"]:
            lines.append("  " + NO_LICENSE_MARKER)
    lines.extend(["", "## License texts", ""])
    for license_id in sorted(used, key=str.casefold):
        path = (licenses_dir / sources[license_id][0]).as_posix()
        lines.append(
            "- **{}** — [`{}`]({}) — <{}>".format(
                license_id, path, path, sources[license_id][1]
            )
        )
    lines.extend(
        [
            "",
            "## MPL-2.0 source availability",
            "",
            "MPL-2.0 applies only to the desktop shell's Tauri "
            "codegen/utility chain ({}).".format(
                ", ".join(mpl) if mpl else "none in this SBOM"
            ),
            "Those crates are not part of the CLI. The complete Corresponding",
            "Source for each exact version is the crates.io package source at",
            "`https://crates.io/crates/<crate>/<version>/source` (also linked",
            "from the SBOM); the MPL-2.0 text is reproduced in",
            "`assets/licenses/component-licenses.txt` and `assets/licenses/MPL-2.0.txt`.",
            "",
            "## Bundled fonts",
            "",
            "The bundled Source Han Sans subset carries the SIL Open Font License",
            "1.1 and is a Modified Version under that license, so it does not use",
            "the Reserved Font Name: its name table identifies the face as",
            "`printkit Han Sans SC` (`printkitHanSansSC-Regular`), and nameID 0",
            "keeps Adobe's copyright statement as attribution. Noto Sans Mono is",
            "distributed unchanged. The font-specific license text travels as",
            "`assets/fonts/OFL.txt`; the standard OFL-1.1 text is also in",
            "`assets/licenses/OFL-1.1.txt`.",
            "",
        ]
    )
    return "\n".join(lines)


def render_licenses(components, data, licenses_dir):
    entries = legal_entries(components, data, licenses_dir)
    texts = data["texts"]
    lines = [
        "# Third-party license texts",
        "",
        "Generated from the CycloneDX SBOM and",
        "`assets/licenses/component-licenses.json` by `scripts/gen-notices.py`;",
        "do not edit this file by hand. Rust data is collected with Mozilla",
        "`cargo-about`; Node data comes from `pnpm licenses list --long` and the",
        "installed package LICENSE files. The inventory summary is in",
        "`THIRD-PARTY-NOTICES.md`.",
        "",
        "Each section reproduces the license files shipped by that exact upstream",
        "package version. Copyright remains with the upstream authors; these",
        "texts do not reassign it. `SPDX:<id>` marks the canonical SPDX text used",
        "when an npm package publishes no license file of its own.",
        "",
        "Apache-2.0 license files contain an appendix template line",
        "`Copyright {yyyy} {name of copyright owner}` (or its square-bracket",
        "variant); it is not a component attribution and is never promoted to",
        "the Copyright field. A component whose packaged metadata has no",
        "copyright line is marked explicitly.",
        "",
        "This appendix is a best-effort disclosure, not a full-compliance claim.",
        "",
    ]
    for component, entry, _ in sorted(entries, key=lambda item: sort_key(item[0])):
        lines.extend(
            [
                "## {} `{}`".format(field(entry["name"]), field(entry["version"])),
                "",
                "- License expression: `{}`".format(entry["license"]),
            ]
        )
        if entry["copyright"]:
            lines.append("- Copyright: " + " / ".join(entry["copyright"]))
        else:
            lines.append("- Copyright: " + NO_COPYRIGHT_MARKER)
        if entry.get("author"):
            lines.append(
                "- Package author metadata (not a copyright statement): {}".format(
                    entry["author"]
                )
            )
        lines.append("")
        if not entry["texts"]:
            lines.extend(["[no license text found in the upstream package; see source]", ""])
        for reference in entry["texts"]:
            lines.extend(
                [
                    "### {}".format(reference["id"]),
                    "",
                    "- Source: `{}`".format(reference["source"]),
                    "",
                    fs_fence(texts[reference["sha256"]]),
                    "",
                ]
            )
        for reference in entry["notices"]:
            lines.extend(
                [
                    "### NOTICE file",
                    "",
                    "- Source: `{}`".format(reference["source"]),
                    "",
                    fs_fence(texts[reference["sha256"]]),
                    "",
                ]
            )
    return "\n".join(lines)


def write_outputs(args, components, data, sources):
    notices = render_notices(components, data, Path(args.licenses_dir), sources)
    licenses = render_licenses(components, data, Path(args.licenses_dir))
    Path(args.output).write_text(notices, encoding="utf-8")
    Path(args.licenses_output).write_text(licenses, encoding="utf-8")
    third_party = len([c for c in components if not first_party(c)])
    print(
        "notices: wrote {} and {} ({} components, {} third-party)".format(
            args.output, args.licenses_output, len(components), third_party
        )
    )


# ---------------------------------------------------------------------------
# Entry point


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sbom", default="dist/sbom.cdx.json")
    parser.add_argument("--output", default="THIRD-PARTY-NOTICES.md")
    parser.add_argument("--licenses-output", default=LICENSES_NAME)
    parser.add_argument("--licenses-dir", default=str(LICENSE_DIR))
    parser.add_argument("--check", action="store_true")
    parser.add_argument(
        "--check-cargo",
        action="store_true",
        help="check a fresh Cargo-only SBOM against the committed inventory",
    )
    parser.add_argument(
        "--rebuild-legal-data",
        action="store_true",
        help="rebuild component-licenses.json from cargo-about and pnpm output",
    )
    parser.add_argument("--about-cli", help="cargo-about JSON for the CLI workspace")
    parser.add_argument(
        "--about-desktop", help="cargo-about JSON for the desktop workspace"
    )
    parser.add_argument("--pnpm-json", help="pnpm licenses list --long --json output")
    parser.add_argument("--about-version", default="0.9.3")
    args = parser.parse_args()

    licenses_dir = Path(args.licenses_dir)
    if args.check and args.check_cargo:
        raise SystemExit("--check and --check-cargo are mutually exclusive")

    sbom = read_json(args.sbom, "SBOM")
    components = sbom.get("components") or []
    versions = workspace_versions()

    if args.check_cargo:
        try:
            actual = Path(args.output).read_text(encoding="utf-8")
        except OSError as error:
            raise SystemExit("cannot read {}: {}".format(args.output, error))
        expected_lines = [
            component_line(c, versions) for c in components if not first_party(c)
        ]
        missing = [line for line in expected_lines if line not in actual]
        if missing:
            print(
                "{} is stale for {} Cargo component(s); first: {}".format(
                    args.output, len(missing), missing[0]
                ),
                file=sys.stderr,
            )
            raise SystemExit(1)
        print(
            "notices: cargo ok ({} third-party Cargo components present)".format(
                len(expected_lines)
            )
        )
        return

    sources = load_sources(licenses_dir)

    if args.rebuild_legal_data:
        for option in ("about_cli", "about_desktop", "pnpm_json"):
            if not getattr(args, option):
                raise SystemExit("--rebuild-legal-data requires --{}".format(
                    option.replace("_", "-")
                ))
        rebuild_legal_data(args)
        data_path = licenses_dir / LEGAL_DATA_NAME
        data = read_json(data_path, "legal data")
        write_outputs(args, components, data, sources)
        return

    data_path = licenses_dir / LEGAL_DATA_NAME
    if not data_path.is_file():
        raise SystemExit(
            "{} is missing; run scripts/gen-legal-notices.sh".format(data_path)
        )
    data = read_json(data_path, "legal data")

    if args.check:
        expected_notices = render_notices(components, data, licenses_dir, sources)
        expected_licenses = render_licenses(components, data, licenses_dir)
        for path, expected in ((args.output, expected_notices), (args.licenses_output, expected_licenses)):
            try:
                actual = Path(path).read_text(encoding="utf-8")
            except OSError as error:
                raise SystemExit("cannot read {}: {}".format(path, error))
            if actual != expected:
                print(
                    "{} is stale; run scripts/gen-legal-notices.sh".format(path),
                    file=sys.stderr,
                )
                raise SystemExit(1)
        entries = legal_entries(components, data, licenses_dir)
        entries_by_text = [entry for _, entry, _ in entries if entry["texts"]]
        third_party = len(entries)
        with_copyright = sum(1 for _, entry, _ in entries if entry["copyright"])
        notices = sum(len(entry["notices"]) for _, entry, _ in entries)
        print(
            "notices: ok ({} components, {} third-party, {} with copyright lines, "
            "{} without, {} license texts, {} NOTICE files)".format(
                len(components),
                third_party,
                with_copyright,
                third_party - with_copyright,
                sum(len(entry["texts"]) for _, entry, _ in entries),
                notices,
            )
        )
        return

    write_outputs(args, components, data, sources)


if __name__ == "__main__":
    main()
