#!/usr/bin/env python3
"""Check glibc requirements in the extracted consumer package, not the build tree."""
import argparse
import pathlib
import re
import shutil
import subprocess
import tempfile

BASELINE = (2, 35)
REQUIREMENT = re.compile(r"GLIBC_([0-9]+(?:\.[0-9]+)+)")
FILES = ("nestweaver", "lib/libstdc++.so.6", "lib/libgcc_s.so.1")


def verify(root):
    for name in FILES:
        artifact = root / name
        if not artifact.is_file() or not artifact.resolve().is_relative_to(root.resolve()):
            raise ValueError(f"missing or escaping packaged artifact: {name}")
        result = subprocess.run(["readelf", "--version-info", str(artifact)],
                                capture_output=True, text=True, check=True)
        versions = {tuple(map(int, value.split(".")))
                    for value in REQUIREMENT.findall(result.stdout)}
        if not versions:
            raise ValueError(f"no readable glibc requirement in {name}")
        maximum = max(versions)
        if maximum > BASELINE:
            raise ValueError(f"{name} requires GLIBC_{'.'.join(map(str, maximum))}; "
                             "declared ceiling is GLIBC_2.35")
        print(f"{name}: maximum GLIBC_{'.'.join(map(str, maximum))}")


def negative_control(root):
    # Change one real ELF dynamic version string in a disposable copy, keeping
    # its width/section layout intact. readelf still parses the actual ELF
    # version-needs table; the verifier must refuse the injected requirement.
    with tempfile.TemporaryDirectory(prefix="nw-archive-negative-") as tmp:
        fixture = pathlib.Path(tmp) / "archive"
        shutil.copytree(root, fixture)
        binary = fixture / "nestweaver"
        data = binary.read_bytes()
        match = re.search(rb"GLIBC_[0-9]\.[0-9]{2}\x00", data)
        if not match:
            raise ValueError("cannot construct an ELF version-needs negative control")
        binary.write_bytes(data.replace(match[0], b"GLIBC_9.99\x00"))
        try:
            verify(fixture)
        except ValueError as error:
            if "GLIBC_9.99" not in str(error):
                raise
            print(f"negative control rejected: {error}")
        else:
            raise ValueError("over-baseline ELF was accepted")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extracted_archive", type=pathlib.Path)
    parser.add_argument("--negative-control", action="store_true")
    args = parser.parse_args()
    try:
        verify(args.extracted_archive)
        if args.negative_control:
            negative_control(args.extracted_archive)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Linux archive verification failed: {error}\n")
