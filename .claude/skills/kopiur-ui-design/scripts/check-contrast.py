#!/usr/bin/env python3
"""Check every kopiur-ui-design token pair against WCAG contrast, in both themes.

Parses the CSS block in ../references/tokens.md (the single source), resolves each
`light-dark(light, dark)` colour, and checks the pairs the console actually draws.
Exits 1 if any pair fails, so it can gate a token change.

    python3 .claude/skills/kopiur-ui-design/scripts/check-contrast.py
"""
import pathlib
import re
import sys

TOKENS = pathlib.Path(__file__).resolve().parent.parent / "references" / "tokens.md"
KINDS = ["repository", "cluster-repository", "maintenance", "snapshot-policy", "snapshot-schedule",
         "snapshot", "restore", "repository-replication", "snapshot-replication"]
STATES = ["healthy", "failed", "degraded", "pending", "unknown"]


def load():
    css = re.search(r"```css\n(.*?)```", TOKENS.read_text(), re.S).group(1)
    out = {}
    for name, a, b in re.findall(r"--([\w-]+):\s*light-dark\((#[0-9a-fA-F]{6}),\s*(#[0-9a-fA-F]{6})\)", css):
        out[name] = (a, b)
    return out


def lum(h):
    r, g, b = (int(h[i:i + 2], 16) / 255 for i in (1, 3, 5))
    f = lambda c: c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4
    return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)


def ratio(a, b):
    hi, lo = sorted((lum(a), lum(b)), reverse=True)
    return (hi + 0.05) / (lo + 0.05)


def pairs():
    text, ui = 4.5, 3.0
    yield "fg on canvas", "fg", "bg-canvas", text
    yield "fg on surface", "fg", "bg-surface", text
    yield "fg-muted on canvas", "fg-muted", "bg-canvas", text
    yield "fg-muted on surface", "fg-muted", "bg-surface", text
    yield "fg-muted on inset", "fg-muted", "bg-inset", text
    yield "accent-ink on canvas", "accent-ink", "bg-canvas", text
    yield "accent-ink on surface", "accent-ink", "bg-surface", text
    yield "on-accent on accent (primary button)", "fg-on-accent", "accent", text
    yield "accent-ink on accent-soft (soft button, current nav)", "accent-ink", "accent-soft", text
    yield "accent focus ring on surface", "accent", "bg-surface", ui
    for s in STATES:
        yield f"{s} pill", f"health-{s}-fg", f"health-{s}-bg", text
    yield "failed fg as loud text on surface", "health-failed-fg", "bg-surface", text
    yield "degraded fg on degraded bg (not-permitted callout)", "health-degraded-fg", "health-degraded-bg", text
    for k in KINDS:
        yield f"{k}: small-caps name on surface", f"kind-{k}", "bg-surface", text
        yield f"{k}: glyph on chip", f"kind-{k}", f"kind-{k}-bg", ui
        yield f"{k}: stripe on canvas", f"kind-{k}", "bg-canvas", ui


def main():
    t = load()
    fails = 0
    for label, fg, bg, need in pairs():
        for i, theme in enumerate(("light", "dark")):
            r = ratio(t[fg][i], t[bg][i])
            ok = r >= need
            fails += not ok
            print(f"{'ok  ' if ok else 'FAIL'} {r:5.2f} ≥ {need}  {theme:5}  {label}")
    print(f"\n{fails} failing pair(s)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
