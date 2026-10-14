"""THIRD_PARTY_LICENSES.txt for the `ultron` binary from `cargo metadata --format-version 1 --filter-platform <t>`
on stdin: every registry or git crate the `cli` package links (normal dependencies only; build scripts, proc
macros and dev dependencies are not distributed), with its license expression and the license and notice files
it ships.

Usage: cargo metadata ... | python3 -I third_party_licenses.py > THIRD_PARTY_LICENSES.txt
Exits 1 when a linked crate has no license expression and no license file.
"""

import json
import os
import re
import sys

LICENSE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|NOTICE|COPYRIGHT|UNLICENSE)", re.IGNORECASE)


def main() -> int:
    metadata = json.load(sys.stdin)
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root = next(id for id, package in packages.items() if package["name"] == "cli" and package["source"] is None)

    def proc_macro(package: dict) -> bool:
        return any("proc-macro" in target["kind"] for target in package["targets"])

    # Proc macros run in the compiler; neither they nor their own dependencies end up in the binary.
    linked, stack = set(), [root]
    while stack:
        id = stack.pop()
        if id in linked or proc_macro(packages[id]):
            continue
        linked.add(id)
        for dep in nodes[id]["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    external = sorted(
        (packages[id] for id in linked if packages[id]["source"] is not None),
        key=lambda package: (package["name"], package["version"]),
    )
    out, missing = [], []
    out.append("Third-party Rust crates compiled into ultron\n" + "=" * 44 + "\n")
    out.append("".join(f"- {p['name']} {p['version']}: {p.get('license') or 'see file'}\n" for p in external))
    for package in external:
        directory = os.path.dirname(package["manifest_path"])
        files = sorted(name for name in os.listdir(directory) if LICENSE_FILE.match(name))
        if package.get("license_file"):
            files = sorted(set(files) | {os.path.relpath(os.path.join(directory, package["license_file"]), directory)})
        if not files and not package.get("license"):
            missing.append(f"{package['name']} {package['version']}")
        out.append("\n" + "-" * 79 + f"\n{package['name']} {package['version']} ({package.get('license') or 'license file'})\n")
        if package.get("repository"):
            out.append(f"{package['repository']}\n")
        out.append("-" * 79 + "\n")
        for name in files:
            path = os.path.join(directory, name)
            if os.path.isfile(path):
                with open(path, encoding="utf-8", errors="replace") as handle:
                    out.append(f"\n[{name}]\n\n{handle.read().rstrip()}\n")
        if not files:
            out.append("\n(no license file in the published crate; licensed under the expression above)\n")
    sys.stdout.write("".join(out))
    if missing:
        print("crates without any license information: " + ", ".join(missing), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
