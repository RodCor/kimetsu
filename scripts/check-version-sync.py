#!/usr/bin/env python3
"""Fail when the workspace version, crate pins, npm packages, or publish order drift.

The workspace version in the root Cargo.toml is the release number. This script
checks every place that number is copied:

- path-dependency version pins in crates/*/Cargo.toml
- workspace package versions in Cargo.lock
- npm/kimetsu/package.json and npm/kimetsu-remote/package.json, including
  optionalDependencies (the platform packages are generated at publish time
  from this same number by scripts/assemble-npm.sh)
- npm/kimetsu-sdk/package.json and the matching package-lock entries
- the vX.Y.Z heading in CHANGELOG.md
- the `cargo publish -p` order in .github/workflows/release.yml

kimetsu-bus's dev-dependency on kimetsu-sdk is path-only. Cargo drops a
versionless path dev-dependency from the published manifest, which is what
lets the bus upload before the SDK. A version on that line is a failure.

Release and npm builds of the kimetsu binary pass the `network` feature.
Every `extra_features` value in the release matrix must include it. npm
packages the lean archive those builds produce.
"""

from __future__ import annotations

import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PUBLISH_ORDER = (
    "kimetsu-core",
    "kimetsu-brain",
    "kimetsu-agent",
    "kimetsu-bus",
    "kimetsu-sdk",
    "kimetsu-chat",
    "kimetsu-cli",
    "kimetsu-remote",
)
NPM_MANIFESTS = (
    "npm/kimetsu/package.json",
    "npm/kimetsu-remote/package.json",
    "npm/kimetsu-sdk/package.json",
)
DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def main() -> int:
    errors: list[str] = []
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
    version = workspace["workspace"]["package"]["version"]
    members = [Path(member).name for member in workspace["workspace"]["members"]]

    manifests: dict[str, dict] = {}
    for member in members:
        path = ROOT / "crates" / member / "Cargo.toml"
        manifest = tomllib.loads(path.read_text())
        manifests[member] = manifest
        package = manifest["package"]
        pkg_version = package.get("version")
        if isinstance(pkg_version, dict):
            if pkg_version.get("workspace") is not True:
                errors.append(f"{path.relative_to(ROOT)} version is not workspace = true")
        elif pkg_version != version:
            errors.append(f"{path.relative_to(ROOT)} version {pkg_version} != {version}")
        for table in DEP_TABLES:
            for name, spec in manifest.get(table, {}).items():
                if not isinstance(spec, dict) or not str(name).startswith("kimetsu-"):
                    continue
                pinned = spec.get("version")
                path_only_bus_sdk = (
                    member == "kimetsu-bus"
                    and table == "dev-dependencies"
                    and name == "kimetsu-sdk"
                )
                if path_only_bus_sdk:
                    if pinned is not None:
                        errors.append(
                            "crates/kimetsu-bus/Cargo.toml dev-dependency kimetsu-sdk "
                            "must stay path-only (no version) so cargo publish can "
                            "upload kimetsu-bus before kimetsu-sdk"
                        )
                    if spec.get("path") != "../kimetsu-sdk":
                        errors.append("kimetsu-bus dev-dependency kimetsu-sdk lost its path")
                    continue
                if pinned != version:
                    errors.append(
                        f"crates/{member}/Cargo.toml [{table}] {name} version {pinned!r} != {version}"
                    )

    publishable = [
        name
        for name, manifest in manifests.items()
        if manifest["package"].get("publish") is not False
    ]
    if sorted(publishable) != sorted(PUBLISH_ORDER):
        errors.append(
            f"publishable crates {sorted(publishable)} != publish order {list(PUBLISH_ORDER)}"
        )
    if "kimetsu-e2e" not in members or manifests["kimetsu-e2e"]["package"].get("publish") is not False:
        errors.append("kimetsu-e2e must stay publish = false and out of the crates.io job")

    for index, crate in enumerate(PUBLISH_ORDER):
        for dep in versioned_kimetsu_deps(manifests[crate]):
            if dep not in PUBLISH_ORDER:
                errors.append(f"{crate} version-depends on {dep}, which is not published")
                continue
            if PUBLISH_ORDER.index(dep) >= index:
                errors.append(f"publish {crate} before {dep} is published")

    check_lock(version, members, errors)
    check_npm(version, errors)
    check_changelog(version, errors)
    check_release_workflow(errors)

    if errors:
        print("version sync failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(f"version sync ok ({version})")
    return 0


def versioned_kimetsu_deps(manifest: dict) -> list[str]:
    found: list[str] = []
    for table in DEP_TABLES:
        for name, spec in manifest.get(table, {}).items():
            if isinstance(spec, dict) and str(name).startswith("kimetsu-") and "version" in spec:
                found.append(str(name))
    return found


def check_lock(version: str, members: list[str], errors: list[str]) -> None:
    wanted = set(members)
    seen: set[str] = set()
    name = None
    source = False
    pkg_version = None
    for line in (ROOT / "Cargo.lock").read_text().splitlines():
        if line == "[[package]]":
            finish_lock_package(name, source, pkg_version, version, wanted, seen, errors)
            name, source, pkg_version = None, False, None
            continue
        if line.startswith("name = "):
            name = line.split("=", 1)[1].strip().strip('"')
        elif line.startswith("version = ") and pkg_version is None:
            pkg_version = line.split("=", 1)[1].strip().strip('"')
        elif line.startswith("source = "):
            source = True
    finish_lock_package(name, source, pkg_version, version, wanted, seen, errors)
    missing = wanted - seen
    if missing:
        errors.append(f"Cargo.lock is missing workspace packages: {sorted(missing)}")


def finish_lock_package(
    name: str | None,
    source: bool,
    pkg_version: str | None,
    version: str,
    wanted: set[str],
    seen: set[str],
    errors: list[str],
) -> None:
    if name is None or name not in wanted or source:
        return
    seen.add(name)
    if pkg_version != version:
        errors.append(f"Cargo.lock {name} version {pkg_version} != {version}")


def check_npm(version: str, errors: list[str]) -> None:
    for rel in NPM_MANIFESTS:
        data = json.loads((ROOT / rel).read_text())
        if data.get("version") != version:
            errors.append(f"{rel} version {data.get('version')} != {version}")
        for name, pinned in data.get("optionalDependencies", {}).items():
            if pinned != version:
                errors.append(f"{rel} optionalDependencies {name}={pinned} != {version}")
    lock = json.loads((ROOT / "npm/kimetsu-sdk/package-lock.json").read_text())
    if lock.get("version") != version:
        errors.append(f"npm/kimetsu-sdk/package-lock.json version {lock.get('version')} != {version}")
    root_pkg = lock.get("packages", {}).get("")
    if not isinstance(root_pkg, dict) or root_pkg.get("version") != version:
        errors.append("npm/kimetsu-sdk/package-lock.json packages[''] version drifted")

    assemble = (ROOT / "scripts/assemble-npm.sh").read_text()
    if assemble.count("j.version = version") != 2 or assemble.count("j.optionalDependencies[k] = version") != 2:
        errors.append("scripts/assemble-npm.sh must stamp both wrappers and their optionalDependencies")
    if "kimetsu-${VERSION}-${target}-lean" not in assemble:
        errors.append("scripts/assemble-npm.sh must package the lean kimetsu archive")


def check_changelog(version: str, errors: list[str]) -> None:
    changelog = (ROOT / "CHANGELOG.md").read_text()
    if not re.search(rf"^## v{re.escape(version)}(:|\s|$)", changelog, re.M):
        errors.append(f"CHANGELOG.md has no heading for v{version}")


def check_release_workflow(errors: list[str]) -> None:
    workflow = (ROOT / ".github/workflows/release.yml").read_text()
    published = re.findall(r"cargo publish -p ([A-Za-z0-9_-]+)", workflow)
    if tuple(published) != PUBLISH_ORDER:
        errors.append(f"release.yml publish order {published} != {list(PUBLISH_ORDER)}")
    features = re.findall(r'extra_features:\s*"([^"]*)"', workflow)
    if len(features) < 7:
        errors.append(f"release.yml has {len(features)} extra_features entries; expected the binary matrix")
    for entry in features:
        tokens = set(re.split(r"[\s,]+", entry))
        if "network" not in tokens:
            errors.append(f"release build is missing the network feature: {entry}")
    if not any("--no-default-features" in entry and "network" in entry for entry in features):
        errors.append("no lean release build enables the network feature")


if __name__ == "__main__":
    sys.exit(main())
