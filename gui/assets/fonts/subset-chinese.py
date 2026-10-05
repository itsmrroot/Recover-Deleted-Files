#!/usr/bin/env python3
"""Cuts Noto Sans SC down to the characters the Chinese translation uses.

The full fonts are 8 MB each; the app only needs the few hundred characters
in gui/src/translations.rs, so the bundled copies are a few hundred KB. Run
this again whenever the Chinese translation changes (a test in i18n.rs fails
if a character is missing):

    pip install fonttools
    curl -LO https://github.com/notofonts/noto-cjk/raw/main/Sans/SubsetOTF/SC/NotoSansSC-Regular.otf
    curl -LO https://github.com/notofonts/noto-cjk/raw/main/Sans/SubsetOTF/SC/NotoSansSC-Medium.otf
    python3 gui/assets/fonts/subset-chinese.py NotoSansSC-Regular.otf NotoSansSC-Medium.otf
"""

import re
import sys
from pathlib import Path

from fontTools import subset

HERE = Path(__file__).resolve().parent
TRANSLATIONS = HERE.parent.parent / "src" / "translations.rs"
# Shown in every language's picker (see `Language::label` in i18n.rs).
LANGUAGE_NAME = "简体中文"


def chinese_text() -> str:
    src = TRANSLATIONS.read_text(encoding="utf-8")
    table = src[src.index("pub const CHINESE"):]
    table = table[: table.index("];")]
    values = re.findall(r'"(?:[^"\\]|\\.)*",\s*"((?:[^"\\]|\\.)*)"', table)
    # Latin letters, digits and punctuation come from the system font.
    return "".join(sorted({c for v in values + [LANGUAGE_NAME] for c in v if ord(c) > 0x2000}))


def main() -> None:
    text = chinese_text()
    for source in sys.argv[1:]:
        weight = "Medium" if "Medium" in source else "Regular"
        out = HERE / f"NotoSansSC-{weight}-subset.otf"
        options = subset.Options()
        options.name_IDs = ["*"]
        options.notdef_outline = True
        font = subset.load_font(source, options)
        subsetter = subset.Subsetter(options)
        subsetter.populate(text=text)
        subsetter.subset(font)
        subset.save_font(font, str(out), options)
        print(f"{out.name}: {len(text)} characters, {out.stat().st_size // 1024} KB")


if __name__ == "__main__":
    main()
