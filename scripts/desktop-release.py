#!/usr/bin/env python3
"""UP1: version and publish one complete private macOS update after main CI."""
import argparse
import datetime
import json
import os
import plistlib
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
REPO = "oppenxyz/oppen"


def version(run):
    if not re.fullmatch(r"[1-9][0-9]*", run):
        raise ValueError("Expected a positive CI run number")
    return f"0.1.{run}"


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True).strip()


def prepare(run):
    value = version(run)
    config = ROOT / "apps/desktop/src-tauri/tauri.conf.json"
    data = json.loads(config.read_text())
    data["version"] = value
    config.write_text(json.dumps(data, indent=2) + "\n")
    print(value)


def metadata(value, url, signature):
    prefix = f"https://api.github.com/repos/{REPO}/releases/assets/"
    if not url.startswith(prefix) or not url[len(prefix):].isdigit() or not signature.strip():
        raise ValueError("Invalid signed release asset")
    return {"version": value, "notes": "Oppen main update. Install when ready to restart.",
            "pub_date": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "platforms": {"darwin-aarch64": {"url": url, "signature": signature.strip()}}}


def publish(run):
    value = version(run)
    tag = f"desktop-v{value}"
    sha = os.environ["GITHUB_SHA"]
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("Expected the tested commit SHA")
    releases = json.loads(gh("api", f"repos/{REPO}/releases", "--paginate", "--slurp"))
    releases = [item for page in releases for item in page]
    # CI completion order is not commit order. Never promote an older version.
    published = [int(r["tag_name"].removeprefix("desktop-v0.1.")) for r in releases
                 if not r["draft"] and not r["prerelease"] and re.fullmatch(r"desktop-v0\.1\.[0-9]+", r["tag_name"])]
    if published and max(published) >= int(run):
        print("This run is already published or superseded.")
        return
    bundle = ROOT / "target/aarch64-apple-darwin/release/bundle/macos"
    with (bundle / "oppen.app/Contents/Info.plist").open("rb") as source:
        if plistlib.load(source)["CFBundleShortVersionString"] != value:
            raise ValueError("Bundle version does not match this release run")
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle / "oppen.app")], check=True)
    archives = list(bundle.glob("*.app.tar.gz"))
    if len(archives) != 1:
        raise ValueError("Expected one updater archive")
    archive = archives[0]
    signature = Path(str(archive) + ".sig")
    if not signature.is_file():
        raise ValueError("Missing updater signature")
    existing = next((r for r in releases if r["tag_name"] == tag), None)
    if not existing:
        gh("release", "create", tag, "--repo", REPO, "--target", sha, "--draft",
           "--title", f"Oppen {value}", "--notes", f"Private Apple Silicon build from {sha}. Tauri-signed; not Apple-notarized.")
    gh("release", "upload", tag, str(archive), str(signature), "--repo", REPO, "--clobber")
    release = json.loads(gh("api", f"repos/{REPO}/releases/tags/{tag}"))
    asset = next(a for a in release["assets"] if a["name"] == archive.name)
    manifest = bundle / "latest.json"
    manifest.write_text(json.dumps(metadata(value, asset["url"], signature.read_text()), indent=2) + "\n")
    gh("release", "upload", tag, str(manifest), "--repo", REPO, "--clobber")
    # This is the only visibility switch. Readers never see a partial release.
    gh("release", "edit", tag, "--repo", REPO, "--draft=false", "--latest")
    print(f"Published {tag}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("step", choices=["prepare", "publish"])
    parser.add_argument("run")
    args = parser.parse_args()
    (prepare if args.step == "prepare" else publish)(args.run)
