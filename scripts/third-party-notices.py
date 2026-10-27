#!/usr/bin/env python3
"""Writes THIRD-PARTY-NOTICES.txt: the licences of everything built into the
program (the Rust libraries, and the fonts some of them embed), as their
licences require when the program is passed on.

    python3 scripts/third-party-notices.py          # rewrite the file
    python3 scripts/third-party-notices.py --check  # fail if it is out of date

Run it after changing dependencies (CI runs --check). It reads `cargo
metadata`, so the sources must be downloaded (`cargo fetch --locked`).
"""

import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "THIRD-PARTY-NOTICES.txt"
# Licence files: LICENSE, LICENSE-MIT, COPYING, NOTICE, UNLICENSE, and the
# font licences egui ships next to its fonts (OFL.txt, UFL.txt, ...).
LICENCE_FILE = re.compile(r"^(licen[cs]e|copying|notice|unlicense|copyright|ofl|ufl)|licen[cs]e", re.I)
SKIP_SUFFIX = {".rs", ".toml", ".ttf", ".otf", ".png", ".svg", ".html", ".json", ".yml", ".yaml"}
OWN = {"wdfr", "wdfr-gui"}
# Standard texts (from SPDX's licence list) for libraries that ship no
# licence file of their own; for "A OR B" the first one here is used.
STANDARD = ROOT / "scripts" / "licence-texts"
PREFERRED = ["MIT", "Apache-2.0", "Zlib", "BSL-1.0"]


def chosen_licences(expr: str) -> list[str]:
    """The licences used for an SPDX expression: all of an AND, one of an OR."""
    expr = expr.replace("/", " OR ")
    out = []
    for part in re.split(r"\s+AND\s+", expr.replace("(", "").replace(")", "")):
        options = [o.split(" WITH ")[0].strip() for o in re.split(r"\s+OR\s+", part)]
        pick = next((p for p in PREFERRED if p in options), options[0])
        out.append(pick)
    return out


def licence_files(pkg_dir: Path) -> list[Path]:
    found = []
    for d in (pkg_dir, pkg_dir / "fonts", pkg_dir / "licenses", pkg_dir / "LICENSES"):
        if not d.is_dir():
            continue
        for f in sorted(d.iterdir()):
            # In a fonts folder every text file is a font's licence.
            font_notice = d.name == "fonts" and f.suffix.lower() == ".txt"
            if f.is_file() and f.suffix.lower() not in SKIP_SUFFIX and (LICENCE_FILE.search(f.name) or font_notice):
                found.append(f)
    return found


def main() -> int:
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    packages = sorted(
        (p for p in meta["packages"] if p["name"] not in OWN),
        key=lambda p: (p["name"].lower(), p["version"]),
    )
    texts: dict[str, list[str]] = defaultdict(list)  # licence text -> crates
    standard: dict[str, list[str]] = defaultdict(list)  # SPDX id -> crates
    listing = []
    for p in packages:
        name = f"{p['name']} {p['version']}"
        spdx = p.get("license") or "see licence file"
        listing.append(f"  {name:<44} {spdx}")
        files = licence_files(Path(p["manifest_path"]).parent)
        for f in files:
            text = f.read_text(encoding="utf-8", errors="replace").strip()
            if text:
                texts[text].append(f"{name} ({f.name})")
        if not files:
            if not p.get("license"):
                print(f"{name}: no licence at all", file=sys.stderr)
                return 1
            for lic in chosen_licences(p["license"]):
                if not (STANDARD / f"{lic}.txt").exists():
                    print(f"{name}: add the standard text of {lic} to {STANDARD}", file=sys.stderr)
                    return 1
                standard[lic].append(name)
    out = [
        "Third-party notices",
        "===================",
        "",
        "Deleted Files Recovery is built with the libraries below, and the desktop",
        "app embeds fonts that come with some of them. Each is used under its own",
        "licence, reproduced after the list. The app's own licence is in LICENSE;",
        "the Noto Sans Arabic and Noto Sans SC fonts' licences are in",
        "OFL-NotoSansArabic.txt and OFL-NotoSansSC.txt.",
        "",
        f"Libraries ({len(packages)}):",
        "",
        *listing,
        "",
    ]
    for text, users in sorted(texts.items(), key=lambda t: t[1][0].lower()):
        out += ["", "-" * 78, "Used by: " + ", ".join(users), "-" * 78, "", text, ""]
    for lic, users in sorted(standard.items()):
        text = (STANDARD / f"{lic}.txt").read_text(encoding="utf-8").strip()
        out += ["", "-" * 78, f"{lic}, used by: " + ", ".join(users), "-" * 78, "", text, ""]
    result = "\n".join(out).rstrip() + "\n"
    if "--check" in sys.argv:
        if not OUT.exists() or OUT.read_text(encoding="utf-8") != result:
            print(f"{OUT.name} is out of date: run python3 scripts/third-party-notices.py", file=sys.stderr)
            return 1
        return 0
    OUT.write_text(result, encoding="utf-8")
    print(f"{OUT.name}: {len(packages)} libraries, {len(texts)} distinct licence texts, {len(result) // 1024} KB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
