"""Two untrusted UIDs reuse compiler outputs in the configured system store.

Run after activating the system-store configuration. Uses an isolated home for the second UID,
never its real credentials, and never collects or stops the system store.
"""

import argparse
import json
import os
import pwd
import signal
import subprocess
import tempfile
import uuid
from pathlib import Path

from test_integration import Harness, REPOSITORY as ROOT


def command(arguments, *, expected=0):
    process = subprocess.Popen(
        arguments,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=240)
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.communicate(timeout=20)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
        raise
    result = subprocess.CompletedProcess(arguments, process.returncode, stdout, stderr)
    assert result.returncode == expected, (
        arguments,
        result.returncode,
        result.stderr[-6000:],
    )
    return result


def qualify(bootstrap, second_user, evidence):
    bootstrap = bootstrap.resolve()
    spec = json.loads((bootstrap / "environment.json").read_text())
    second = pwd.getpwnam(second_user)
    assert second.pw_uid not in {0, os.getuid(), 65534}, (
        "Select a distinct normal user UID"
    )
    configuration = Path("/etc/cargo-dyndrv/store.json")
    assert configuration.is_file(), "Activate the Clan cargo-dyndrv-store role first"
    assert json.loads(configuration.read_text())["store_mode"] == "system"
    evidence.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix="dyndrv-multiuser-"))
    command(["sudo", "-n", "chown", "root:root", str(temporary)])
    command(["sudo", "-n", "chmod", "0755", str(temporary)])
    nonce = uuid.uuid4().hex
    records = []
    try:
        for index, (uid, gid) in enumerate(
            ((os.getuid(), os.getgid()), (second.pw_uid, second.pw_gid))
        ):
            home = temporary / f"client-{index}"
            command(["sudo", "-n", "mkdir", str(home)])
            command(["sudo", "-n", "chown", f"{os.getuid()}:{os.getgid()}", str(home)])
            workspace = home / "source"
            workspace.mkdir()
            Harness.copy_tooling(workspace)
            (workspace / "Cargo.toml").write_text(
                '[package]\nname="system-store-probe"\nversion="0.1.0"\nedition="2021"\n[workspace]\n[profile.ci-optim]\ninherits="dev"\nincremental=false\n'
            )
            (workspace / "src").mkdir()
            (workspace / "src/main.rs").write_text(
                f'fn main() {{ println!("shared-{nonce}"); }}\n'
            )
            command(
                [
                    spec["cargo"],
                    "generate-lockfile",
                    "--offline",
                    "--manifest-path",
                    str(workspace / "Cargo.toml"),
                ]
            )
            command(["git", "init", "--quiet", str(workspace)])
            command(["sudo", "-n", "chown", "-R", f"{uid}:{gid}", str(home)])
            command(["sudo", "-n", "chmod", "0700", str(home)])
            prefix = [
                "sudo",
                "-n",
                "-u",
                f"#{uid}",
                "env",
                "-i",
                f"HOME={home}",
                f"XDG_STATE_HOME={home}/state",
                f"CARGO_HOME={home}/cargo",
                "CARGO_DYNDRV_STORE=system",
                "CARGO_INCREMENTAL=0",
                f"PATH={bootstrap}/tools/bin:/run/current-system/sw/bin",
                "NIX_CONFIG=experimental-features = nix-command flakes ca-derivations",
            ]
            result = command(
                prefix
                + [
                    spec["python"],
                    str(workspace / "tests/worker/workload.py"),
                    "--bootstrap",
                    str(bootstrap),
                    "--action",
                    "build",
                    "--",
                    "-p",
                    "system-store-probe",
                ]
            )
            (evidence / f"client-{uid}.stderr").write_text(result.stderr)
            output = command(
                prefix + [str(workspace / "target/ci-optim/system-store-probe")]
            )
            assert output.stdout.strip() == f"shared-{nonce}"
            raw = command(
                ["sudo", "-n", "find", str(home / "state"), "-name", "last-run.json"]
            ).stdout.splitlines()
            assert len(raw) == 1
            last = json.loads(command(["sudo", "-n", "cat", raw[0]]).stdout)
            paths = command(
                ["sudo", "-n", "find", last["session"], "-name", "*.nix-result.json"]
            ).stdout.splitlines()
            receipts = [
                json.loads(command(["sudo", "-n", "cat", path]).stdout)
                for path in paths
            ]
            assert receipts and all(item["compilerSucceeded"] for item in receipts)
            records.append(
                {
                    "uid": uid,
                    "receipts": receipts,
                    "session": last["session"],
                    "logical": last["logical"],
                }
            )
            if index:
                logs = command(
                    ["sudo", "-n", "find", last["session"], "-name", "*.nix-stderr"]
                ).stdout.splitlines()
                for path in logs:
                    for line in command(
                        ["sudo", "-n", "cat", path]
                    ).stdout.splitlines():
                        if line.startswith("@nix "):
                            event = json.loads(line[5:])
                            assert not (
                                event.get("action") == "start"
                                and event.get("type") == 105
                            ), "Second UID started a compiler builder"
            command(
                prefix
                + [
                    spec["python"],
                    str(workspace / "tests/worker/workload.py"),
                    "--bootstrap",
                    str(bootstrap),
                    "--action",
                    "gc",
                    "--",
                ],
                expected=2,
            )
            command(
                prefix
                + [
                    spec["python"],
                    str(workspace / "tests/worker/workload.py"),
                    "--bootstrap",
                    str(bootstrap),
                    "--action",
                    "stop",
                    "--",
                ],
                expected=2,
            )
        first, second_record = records
        assert first["logical"] == second_record["logical"] == "/build/cargo-dyndrv"
        outputs = lambda entry: sorted(
            (item["derivation"], item["output"]) for item in entry["receipts"]
        )
        assert outputs(first) == outputs(second_record), (
            "UID entered compiler request identity"
        )
        (evidence / "validation.json").write_text(
            json.dumps(
                {
                    "passed": True,
                    "clients": records,
                    "temporary": str(temporary),
                    "host_gc": "never requested",
                },
                indent=2,
            )
            + "\n"
        )
    finally:
        command(
            [
                "sudo",
                "-n",
                str(bootstrap / "tools/bin/python3"),
                "-c",
                "import shutil,sys; shutil.rmtree(sys.argv[1])",
                str(temporary),
            ]
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bootstrap", type=Path, required=True)
    parser.add_argument("--second-user", default="codex-dummy-5")
    parser.add_argument(
        "--evidence", type=Path, default=ROOT / "target/dyndrv-validation/system-store"
    )
    args = parser.parse_args()
    qualify(args.bootstrap, args.second_user, args.evidence)
