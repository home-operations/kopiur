#!/usr/bin/env bash
# build-docs.sh — assemble the full Kopiur documentation site.
#
# Produces one directory ready for GitHub Pages:
#   site/            ← MkDocs Material user docs (site root)
#   site/rustdoc/    ← `cargo doc` API reference for the whole workspace
#
# `mkdocs build --strict` fails on a broken intra-site link or a missing nav
# file (validation config in mkdocs.yml), so this script doubles as our doc lint
# (it replaces the old mdbook-linkcheck renderer).
#
# Run via `mise run docs` so uv (and therefore the uv.lock-pinned MkDocs +
# Material + pymdown-extensions) resolves to the versions pinned in the repo.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OUT="site"
# The crate the rustdoc redirect lands on (kopiur-api is the public entry point).
LANDING_CRATE="kopiur_api"
# Custom domain the site is served from. Written into the artifact as a CNAME so
# the domain survives every deploy (must match Settings -> Pages custom domain).
SITE_DOMAIN="kopiur.home-operations.com"

echo "==> cargo doc (workspace, no deps)"
cargo doc --no-deps --workspace --locked

echo "==> checking snippet SECTION and LINE-RANGE references (--strict only guards file paths)"
# pymdownx.snippets' check_paths fails the build on a missing FILE, but a
# `--8<-- "file.yaml:section"` whose section marker was renamed/removed renders
# as a silently EMPTY code block — exactly the docs/manifest drift the snippet
# convention exists to prevent. Verify every section reference resolves.
#
# LINE-RANGE includes (`file.yaml:89:239`) rot the same way, and worse: inserting
# a value anywhere in deploy/helm/kopiur/values.yaml shifts every later range, and
# the result still RENDERS — just half a comment block, silently, with nothing in
# the build to notice. (It went unnoticed for several releases.) A range cannot be
# checked for "shows the intended block", but it can be checked for the structural
# properties every intended block has. Three rules, each earned by an observed
# drift, applied to EVERY line-range include on EVERY page under docs/:
#
#   1. The start line may be a comment only when the line above it is NOT — i.e.
#      it starts a comment block rather than slicing one in half.
#   2. The end line must not be a comment or blank — same rule, other end.
#   3. A start line that is YAML (not a comment) must be at column 0, i.e. a
#      top-level key. Rule 1+2 alone accept a range that begins in the MIDDLE of
#      an indented block — `  minAvailable: 1` — which is exactly how
#      feature-permissions.md ended up captioned "the two flags" over the
#      controller's PodDisruptionBudget.
#
# None of this can prove a range shows the block its prose promises; after any
# values.yaml edit, still re-read the rendered page.
uv run python - <<'PYEOF'
import pathlib, re, sys

errors = []


def check_line_range(md, path, start, end):
    """Structural sanity for a `file:start:end` include; see the note above."""
    lines = path.read_text().splitlines()
    if not (1 <= start <= end <= len(lines)):
        errors.append(f"{md}: range {start}:{end} is outside {path} "
                      f"(the file has {len(lines)} lines)")
        return
    first, last = lines[start - 1], lines[end - 1]
    prev = lines[start - 2] if start >= 2 else ""
    is_comment = first.lstrip().startswith("#")
    if is_comment and prev.lstrip().startswith("#"):
        errors.append(f"{md}: range {start}:{end} STARTS mid-comment in {path} "
                      f"({first.strip()!r}) — the lines almost certainly shifted")
    if not is_comment and first.strip() and first[:1].isspace():
        errors.append(f"{md}: range {start}:{end} STARTS on an INDENTED key in "
                      f"{path} ({first.strip()!r}) — a block include should begin "
                      f"at a top-level key or its comment; the lines almost "
                      f"certainly shifted")
    if not last.strip() or last.lstrip().startswith("#"):
        errors.append(f"{md}: range {start}:{end} ENDS on a comment/blank line in "
                      f"{path} ({last.strip()!r}) — the lines almost certainly shifted")


for md in pathlib.Path("docs").rglob("*.md"):
    for ref in re.findall(r'--8<--\s+"([^":]+):([^"]+)"', md.read_text()):
        path, section = pathlib.Path(ref[0]), ref[1]
        if not path.is_file():
            continue  # missing files are check_paths' job; don't double-report
        if re.fullmatch(r"\d*(:\d*)?", section):
            start, _, end = section.partition(":")
            # Open-ended forms (`:8`, `5:`) are legal in pymdownx; only check a
            # fully-specified range, which is the only form this repo uses.
            if start and end:
                check_line_range(md, path, int(start), int(end))
            continue
        if f"--8<-- [start:{section}]" not in path.read_text():
            errors.append(f"{md}: section '{section}' not found in {path} "
                          f"(expected a '# --8<-- [start:{section}]' marker)")
if errors:
    print("snippet reference check FAILED:", *errors, sep="\n  ", file=sys.stderr)
    sys.exit(1)
PYEOF

echo "==> mkdocs build (--strict: broken link or missing nav file fails here)"
# uv run resolves MkDocs + plugins from the committed uv.lock into a managed venv.
uv run mkdocs build --strict --site-dir "$OUT"

echo "==> nesting rustdoc under ${OUT}/rustdoc"
rm -rf "${OUT}/rustdoc"
cp -r target/doc "${OUT}/rustdoc"

# `cargo doc` on a workspace emits no root index.html, so add a redirect into the
# entry-point crate.
cat > "${OUT}/rustdoc/index.html" <<EOF
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta http-equiv="refresh" content="0; url=${LANDING_CRATE}/index.html">
    <link rel="canonical" href="${LANDING_CRATE}/index.html">
    <title>Kopiur API reference</title>
  </head>
  <body>
    <p>Redirecting to <a href="${LANDING_CRATE}/index.html">the Kopiur API reference</a>…</p>
  </body>
</html>
EOF

# GitHub Pages deploys via Actions do not run Jekyll, but rustdoc emits
# _-prefixed paths; .nojekyll keeps it explicit and future-proof.
touch "${OUT}/.nojekyll"

# Pin the custom domain in the published artifact.
echo "${SITE_DOMAIN}" > "${OUT}/CNAME"

echo "==> docs site assembled at ${OUT}/ (mkdocs + rustdoc) for https://${SITE_DOMAIN}/"
