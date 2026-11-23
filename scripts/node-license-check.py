#!/usr/bin/env python3
"""Check pnpm license reports against the project whitelist.

Usage:
    node-license-check.py REPORT...
    node-license-check.py --components OUT.json REPORT...

Each REPORT is `pnpm licenses list --json` output. With `--components`, the
script also writes the runtime package list as SBOM input. An `Unknown` entry
is a package whose metadata has no license field; only `abbrev` is mapped, from
its upstream npm record.
"""

import json
import sys

ALLOWED = {
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
    "BSL-1.0",
    "OFL-1.1",
    "Unicode-3.0",
}

# npm metadata for abbrev@1.0.3 omits the field; upstream declares ISC.
UNKNOWN = {"abbrev": "ISC"}


def is_allowed(expression):
    expression = expression.replace("(", "").replace(")", "")
    for alternative in expression.split(" OR "):
        terms = [term.strip() for term in alternative.split(" AND ")]
        if all(term in ALLOWED for term in terms):
            return True
    return False


def resolved_license(license_name, package):
    if license_name != "Unknown":
        return license_name, True
    mapped = UNKNOWN.get(package["name"])
    return (mapped, mapped is not None) if mapped else ("Unknown", False)


def main():
    args = sys.argv[1:]
    components_path = None
    if args[:1] == ["--components"]:
        if len(args) < 2:
            raise SystemExit("--components needs an output path")
        components_path = args[1]
        args = args[2:]
    if not args:
        raise SystemExit(__doc__)

    failures = []
    components = []
    license_reports = set()
    for path in args:
        with open(path, encoding="utf-8") as handle:
            report = json.load(handle)
        for license_name, packages in report.items():
            for package in packages:
                license_name, known = resolved_license(license_name, package)
                license_reports.add(license_name)
                if not known or not is_allowed(license_name):
                    failures.append(
                        f"{package['name']}: {license_name} ({path})"
                    )
                    continue
                if components_path:
                    for version in package.get("versions", []):
                        components.append(
                            {
                                "name": package["name"],
                                "version": version,
                                "license": license_name,
                            }
                        )

    if failures:
        print("node licenses: rejected", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        raise SystemExit(1)

    if components_path:
        with open(components_path, "w", encoding="utf-8") as handle:
            json.dump(components, handle, indent=2, sort_keys=True)
            handle.write("\n")
    print(
        "node licenses: ok ({} report(s), {})".format(
            len(args), ", ".join(sorted(license_reports))
        )
    )


if __name__ == "__main__":
    main()
