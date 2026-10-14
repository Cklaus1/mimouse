#!/usr/bin/env python3
"""Render the Scoop, winget and Homebrew manifests for one release.

The files under packaging/{scoop,winget,homebrew} carry the version from
Cargo.toml and @SHA256_<NAME>@ placeholders. This fills the placeholders from
the release artifacts, writes the results to --out (same layout) and, with
--validate, checks them against the official Scoop and winget JSON schemas
(needs network, `jsonschema` and `pyyaml`).

  packaging/render-manifests.py --version 1.0.0 \
      --windows-exe mimouse-windows-x86_64.exe \
      --source-tarball mimouse-1.0.0.tar.gz \
      --out manifests --validate
"""

import argparse
import hashlib
import json
import re
import sys
import tomllib
import urllib.request
from pathlib import Path

PACKAGING = Path(__file__).resolve().parent
CARGO_TOML = PACKAGING.parent / "Cargo.toml"
MANIFEST_DIRS = ("scoop", "winget", "homebrew")
PLACEHOLDER = re.compile(r"@SHA256_([A-Z0-9_]+)@")
SCOOP_SCHEMA = "https://raw.githubusercontent.com/ScoopInstaller/Scoop/master/schema.json"
WINGET_SCHEMA = (
    "https://raw.githubusercontent.com/microsoft/winget-cli/master/"
    "schemas/JSON/manifests/v{v}/manifest.{t}.{v}.json"
)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def render(text: str, hashes: dict[str, str]) -> str:
    def sub(m: re.Match) -> str:
        name = m.group(1)
        if name not in hashes:
            raise SystemExit(f"no artifact for placeholder {m.group(0)}")
        return hashes[name]

    return PLACEHOLDER.sub(sub, text)


def fetch_json(url: str, cache: dict[str, dict]) -> dict:
    if url not in cache:
        with urllib.request.urlopen(url, timeout=30) as r:
            cache[url] = json.load(r)
    return cache[url]


def validate(path: Path, text: str, cache: dict[str, dict]) -> None:
    import jsonschema
    import yaml

    if path.suffix == ".json":
        doc = json.loads(text)
        schema = fetch_json(SCOOP_SCHEMA, cache)
    elif path.suffix == ".yaml":
        # every field we use is a string or a list of strings; BaseLoader keeps
        # versions like 1.0 as strings, as winget reads them
        doc = yaml.load(text, Loader=yaml.BaseLoader)
        schema = fetch_json(
            WINGET_SCHEMA.format(v=doc["ManifestVersion"], t=doc["ManifestType"]), cache
        )
    else:
        return
    jsonschema.validators.validator_for(schema)(schema).validate(doc)
    print(f"valid: {path}")


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--version", required=True, help="release version without the leading v")
    p.add_argument("--windows-exe", type=Path, required=True)
    p.add_argument("--source-tarball", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--validate", action="store_true")
    args = p.parse_args()

    cargo_version = tomllib.loads(CARGO_TOML.read_text())["package"]["version"]
    if args.version != cargo_version:
        raise SystemExit(f"release version {args.version} != Cargo.toml version {cargo_version}")

    hashes = {
        "WINDOWS_X86_64": sha256(args.windows_exe),
        "SOURCE_TARBALL": sha256(args.source_tarball),
    }
    cache: dict[str, dict] = {}
    for d in MANIFEST_DIRS:
        for src in sorted((PACKAGING / d).iterdir()):
            text = src.read_text()
            if args.version not in text:
                raise SystemExit(f"{src} does not mention version {args.version}; update it")
            text = render(text, hashes)
            dst = args.out / d / src.name
            dst.parent.mkdir(parents=True, exist_ok=True)
            dst.write_text(text)
            print(f"rendered: {dst}")
            if args.validate:
                validate(dst, text, cache)
    return 0


if __name__ == "__main__":
    sys.exit(main())
