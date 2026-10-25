"""Test workload only: construct Cargo arguments independently of any client app."""

import argparse
import json
import subprocess
import sys
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--bootstrap", type=Path, required=True)
parser.add_argument("--backend", choices=("cargo", "dyndrv"), default="dyndrv")
parser.add_argument("--action", required=True)
parser.add_argument("arguments", nargs=argparse.REMAINDER)
options = parser.parse_args()
spec = json.loads((options.bootstrap / "environment.json").read_text())
arguments = options.arguments[1:]
if options.action in {"test", "list"}:
    command = [
        "nextest",
        "run" if options.action == "test" else "list",
        "--locked",
        "--offline",
        "--cargo-profile",
        "ci-optim",
        *arguments,
    ]
    command += (
        ["--profile", "ci_dyndrv"]
        if options.action == "test"
        else ["--message-format", "json"]
    )
elif options.action in {"build", "run", "check", "clippy", "doc"}:
    command = [
        options.action,
        "--locked",
        "--offline",
        "--profile",
        "ci-optim",
        *arguments,
    ]
else:
    command = arguments
if options.backend == "cargo":
    invocation = [
        spec["nextest"] if command[:1] == ["nextest"] else spec["cargo"],
        *command,
    ]
else:
    invocation = [
        spec["worker"],
        "--bootstrap",
        str(options.bootstrap),
        "--workspace",
        str(Path.cwd()),
        "--action",
        options.action,
        "--",
        *command,
    ]
status = subprocess.run(invocation, check=False).returncode
sys.exit(status if status >= 0 else 128 - status)
