"""Sign the desktop update index for the two published Quick Bridge ZIPs."""

import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def openssl(*args):
    return subprocess.run(["openssl", *args], check=True, stdout=subprocess.PIPE).stdout


def main(tag, assets):
    public_key = os.environ.get("QUICK_BRIDGE_UPDATE_PUBLIC_KEY", "").strip()
    private_key = os.environ.pop("QUICK_BRIDGE_UPDATE_PRIVATE_KEY", "").strip()
    if not public_key and not private_key:
        print("Update signing is unconfigured. Publishing manually installable packages.")
        return
    if not public_key or not private_key:
        sys.exit("Configure both Quick Bridge update signing keys before publishing.")

    match = re.fullmatch(r"v((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))", tag)
    if not match:
        print("Publishing this tag without a stable desktop update manifest.")
        return
    version = match[1]
    packages = []
    for platform in ("macos-universal", "windows-x64"):
        archive = assets / f"Quick-Bridge-{version}-{platform}.zip"
        with archive.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        packages.append({"platform": platform, "size": archive.stat().st_size, "sha256": digest})

    payload = json.dumps({"version": version, "packages": packages}, separators=(",", ":")).encode()
    with tempfile.TemporaryDirectory() as directory:
        key = Path(directory) / "signing.pem"
        key.write_text(f"{private_key}\n", encoding="utf-8")
        der = openssl("pkey", "-in", key, "-pubout", "-outform", "DER")
        ed25519_prefix = bytes.fromhex("302a300506032b6570032100")
        expected = ed25519_prefix + base64.b64decode(public_key, validate=True)
        if len(der) != len(ed25519_prefix) + 32 or der != expected:
            sys.exit("The Ed25519 signing key does not match the public key embedded in the apps.")
        manifest = Path(directory) / "manifest.json"
        manifest.write_bytes(payload)
        signature = openssl("pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", manifest)

    envelope = {
        "payload": base64.b64encode(payload).decode(),
        "signature": base64.b64encode(signature).decode(),
    }
    (assets / "quick-bridge-update.json").write_text(json.dumps(envelope) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main(sys.argv[1], Path(sys.argv[2]))
