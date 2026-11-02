"""Offline evidence publication, without executing the retained workload."""

import copy
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

REPOSITORY = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY / "worker"))
from evidence import EvidencePacket


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="dyndrv evidence's ")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.root = self.directory / "retained"
        self.root.mkdir()
        self.output = self.directory / "packet.zip"
        self.result = {
            "source": {"commit": "frozen-commit", "tree": "frozen-tree"},
            "run": {"session": "session-id", "uuid": "test-uuid"},
            "compiler": {"nix_invocations": 0, "new_compilation": False},
            "tests": {"pass": 0, "fail": 2, "raw_wait_status": 25600},
            "timing": {"native_seconds": 22.163566, "cpu_seconds": "unknown"},
            "complete": True,
        }
        self.write_result()
        (self.root / "stderr").write_bytes(b"retained failure\n")
        self.spec = {
            "version": 1,
            "scope": "Retained test observation; no scientific approval",
            "files": ["stderr", "result.json"],
            "records": {
                group: {"retained": {"file": "result.json", "pointer": "/" + group}}
                for group in EvidencePacket.RECORDS
            },
            "checks": [{"file": "result.json", "pointer": "/complete", "equals": True}],
        }

    def write_result(self):
        (self.root / "result.json").write_bytes(EvidencePacket.encode(self.result))

    def package(self):
        return EvidencePacket.package(self.spec, self.root, self.output)

    def manifest(self):
        with zipfile.ZipFile(self.output) as archive:
            return json.loads(archive.read("manifest.json"))

    def assert_unpublished(self):
        self.assertFalse(self.output.exists())
        self.assertEqual(list(self.directory.glob("*.partial")), [])

    def rewrite_packet(self, change):
        with zipfile.ZipFile(self.output) as archive:
            entries = [(info, archive.read(info)) for info in archive.infolist()]
        entries = change(entries)
        with zipfile.ZipFile(self.output, "w") as archive:
            for info, contents in entries:
                archive.writestr(info, contents)

    def test_deterministic_archive_preserves_failed_tests_and_reused_compiler(self):
        original = {path.name: path.read_bytes() for path in self.root.iterdir()}
        result = self.package()
        manifest = self.manifest()
        self.assertEqual(
            manifest["records"]["compiler"]["retained"], self.result["compiler"]
        )
        self.assertEqual(manifest["records"]["tests"]["retained"], self.result["tests"])
        self.assertEqual(
            manifest["records"]["timing"]["retained"], self.result["timing"]
        )
        self.assertEqual(
            result["packet_sha256"],
            hashlib.sha256(self.output.read_bytes()).hexdigest(),
        )
        self.assertEqual(result["files"], 2)
        with zipfile.ZipFile(self.output) as archive:
            for name, contents in original.items():
                self.assertEqual(archive.read("files/" + name), contents)
                self.assertEqual((self.root / name).read_bytes(), contents)
        second = self.directory / "another.zip"
        self.spec["files"].reverse()
        os.utime(self.root / "result.json", (100, 100))
        EvidencePacket.package(self.spec, self.root, second)
        self.assertEqual(self.output.read_bytes(), second.read_bytes())
        # Validation is self-contained: it never follows source paths.
        for path in self.root.iterdir():
            path.unlink()
        self.assertEqual(
            EvidencePacket.validate(second)["packet_sha256"], result["packet_sha256"]
        )

    def test_incomplete_false_or_wrong_typed_completion_never_published(self):
        for completion in (False, 1, "true", None):
            with self.subTest(completion=completion):
                self.result["complete"] = completion
                self.write_result()
                with self.assertRaisesRegex(ValueError, "completion check failed"):
                    self.package()
                self.assert_unpublished()

    def test_present_empty_producer_values_are_preserved(self):
        self.result["compiler"]["invocations"] = []
        self.result["timing"]["cpu_seconds"] = None
        self.write_result()
        self.spec["records"]["compiler"] = {
            "invocations": {"file": "result.json", "pointer": "/compiler/invocations"}
        }
        self.spec["records"]["timing"] = {
            "cpu_seconds": {"file": "result.json", "pointer": "/timing/cpu_seconds"}
        }
        self.package()
        records = self.manifest()["records"]
        self.assertEqual(records["compiler"]["invocations"], [])
        self.assertIsNone(records["timing"]["cpu_seconds"])

    def test_missing_file_pointer_or_identity_never_published(self):
        original = copy.deepcopy(self.spec)
        for mutation in ("file", "pointer", "identity", "gate"):
            with self.subTest(mutation=mutation):
                self.spec = copy.deepcopy(original)
                if mutation == "file":
                    self.spec["files"].append("not-yet-written.json")
                elif mutation == "pointer":
                    self.spec["checks"][0]["pointer"] = "/not-yet-written"
                elif mutation == "identity":
                    self.spec["records"]["run"] = {}
                else:
                    self.spec["checks"] = []
                with self.assertRaises((ValueError, OSError)):
                    self.package()
                self.assert_unpublished()

    def test_paths_symlinks_and_special_files_are_rejected(self):
        (self.root / "link").symlink_to(self.root / "result.json")
        (self.root / "linked-directory").symlink_to(self.root, target_is_directory=True)
        os.mkfifo(self.root / "pipe")
        for path in (
            "../escape",
            "/absolute",
            "./stderr",
            "a//b",
            "a\\b",
            "link",
            "linked-directory/result.json",
            "pipe",
        ):
            with self.subTest(path=path):
                spec = {**self.spec, "files": [*self.spec["files"], path]}
                with self.assertRaises(ValueError):
                    EvidencePacket.package(spec, self.root, self.output)
                self.assert_unpublished()

    def test_existing_or_racing_destination_is_preserved(self):
        self.output.write_bytes(b"someone else's output")
        with self.assertRaises(FileExistsError):
            self.package()
        self.assertEqual(self.output.read_bytes(), b"someone else's output")
        self.output.unlink()
        original = EvidencePacket.validate

        def race(packet):
            result = original(packet)
            self.output.write_bytes(b"concurrent publisher")
            return result

        with (
            mock.patch.object(EvidencePacket, "validate", side_effect=race),
            self.assertRaises(FileExistsError),
        ):
            self.package()
        self.assertEqual(self.output.read_bytes(), b"concurrent publisher")
        self.assertEqual(list(self.directory.glob("*.partial")), [])

    def test_source_mutation_before_publication_is_detected(self):
        original = EvidencePacket.validate

        def mutate(packet):
            result = original(packet)
            self.assertFalse(self.output.exists())
            (self.root / "stderr").write_bytes(b"still being written")
            return result

        with (
            mock.patch.object(EvidencePacket, "validate", side_effect=mutate),
            self.assertRaisesRegex(ValueError, "changed during packaging"),
        ):
            self.package()
        self.assert_unpublished()

    def test_source_replacement_before_open_is_detected(self):
        original = EvidencePacket.source

        def replace(root, name):
            path, fingerprint = original(root, name)
            if name == "stderr":
                path.rename(self.root / "old-stderr")
                path.write_bytes(b"new inode")
            return path, fingerprint

        with (
            mock.patch.object(EvidencePacket, "source", side_effect=replace),
            self.assertRaisesRegex(ValueError, "changed before reading"),
        ):
            self.package()
        self.assert_unpublished()

    def test_source_mutation_during_copy_is_detected(self):
        original = EvidencePacket.transfer

        def modify(source, destination=None):
            result = original(source, destination)
            if destination is not None and source.name == str(self.root / "stderr"):
                (self.root / "stderr").write_bytes(b"unfinished new content")
            return result

        with (
            mock.patch.object(EvidencePacket, "transfer", side_effect=modify),
            self.assertRaisesRegex(ValueError, "changed while reading"),
        ):
            self.package()
        self.assert_unpublished()

    def test_hashes_and_projected_records_are_rechecked(self):
        self.package()
        for changed in ("files/stderr", "manifest.json"):
            with self.subTest(changed=changed):

                def change(entries, changed=changed):
                    result = []
                    for info, contents in entries:
                        if info.filename == changed:
                            if changed == "manifest.json":
                                data = json.loads(contents)
                                data["records"]["tests"]["retained"]["fail"] = 0
                                contents = EvidencePacket.encode(data)
                            else:
                                contents = b"different bytes"
                        result.append((info, contents))
                    return result

                self.rewrite_packet(change)
                with self.assertRaisesRegex(
                    ValueError, "manifest contents or file hashes"
                ):
                    EvidencePacket.validate(self.output)
                self.output.unlink()
                self.package()

    def test_validation_hash_cannot_switch_to_another_or_modified_file(self):
        original = EvidencePacket.transfer
        for replacement in (True, False):
            with self.subTest(replacement=replacement):
                self.package()

                def change_at_hash(source, destination=None, replacement=replacement):
                    if getattr(source, "name", None) == str(self.output):
                        if replacement:
                            self.output.rename(self.directory / "original.zip")
                        self.output.write_bytes(b"not a validated archive")
                    return original(source, destination)

                with (
                    mock.patch.object(
                        EvidencePacket, "transfer", side_effect=change_at_hash
                    ),
                    self.assertRaisesRegex(
                        ValueError, "packet changed during validation"
                    ),
                ):
                    EvidencePacket.validate(self.output)
                self.output.unlink()

    def test_completion_gates_are_rechecked_even_with_recomputed_hashes(self):
        self.spec["checks"].append(
            {"file": "result.json", "pointer": "/run/session", "equals": "session-id"}
        )
        self.package()
        changed_result = {**self.result, "complete": False}
        result_bytes = EvidencePacket.encode(changed_result)
        manifest = self.manifest()
        for item in manifest["files"]:
            if item["path"] == "result.json":
                item.update(
                    size=len(result_bytes),
                    sha256=hashlib.sha256(result_bytes).hexdigest(),
                )

        def change(entries):
            return [
                (
                    info,
                    result_bytes
                    if info.filename == "files/result.json"
                    else EvidencePacket.encode(manifest)
                    if info.filename == "manifest.json"
                    else contents,
                )
                for info, contents in entries
            ]

        self.rewrite_packet(change)
        with self.assertRaisesRegex(ValueError, "completion check failed"):
            EvidencePacket.validate(self.output)

    def test_extra_or_duplicate_archive_entries_fail(self):
        self.package()
        self.rewrite_packet(
            lambda entries: entries + [(EvidencePacket.entry("undeclared"), b"extra")]
        )
        with self.assertRaisesRegex(ValueError, "inventory"):
            EvidencePacket.validate(self.output)
        self.output.unlink()
        self.package()
        with self.assertWarns(UserWarning):
            self.rewrite_packet(lambda entries: entries + entries[:1])
        with self.assertRaisesRegex(ValueError, "inventory"):
            EvidencePacket.validate(self.output)

    def test_strict_json_and_pointer_escaping(self):
        for value in (
            b'{"version": 1, "version": 1}',
            b'{"value": NaN}',
            b'{"value": 1e999}',
        ):
            with self.subTest(value=value), self.assertRaises(ValueError):
                EvidencePacket.decode(value)
        self.assertEqual(
            EvidencePacket.select({"a/b": {"~": [True]}}, "/a~1b/~0/0"), True
        )
        for pointer in ("/missing", "/a~1b/~0/01", "/a~1b/~0/-"):
            with self.subTest(pointer=pointer), self.assertRaises(ValueError):
                EvidencePacket.select({"a/b": {"~": [True]}}, pointer)

    def test_cli_reports_packaging_time_separately(self):
        spec = self.directory / "spec.json"
        spec.write_bytes(EvidencePacket.encode(self.spec))
        command = [sys.executable, str(REPOSITORY / "worker/evidence.py")]
        result = subprocess.run(
            command
            + [
                "package",
                "--spec",
                str(spec),
                "--root",
                str(self.root),
                "--output",
                str(self.output),
            ],
            capture_output=True,
            check=False,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads(result.stdout)
        self.assertGreater(receipt["wall_seconds"], 0)
        self.assertGreaterEqual(receipt["cpu_seconds"], 0)
        self.assertEqual(receipt["operation"], "package")
        checked = subprocess.run(
            command + ["validate", str(self.output)],
            capture_output=True,
            check=False,
            text=True,
        )
        self.assertEqual(checked.returncode, 0, checked.stderr)
        self.assertEqual(
            json.loads(checked.stdout)["packet_sha256"], receipt["packet_sha256"]
        )


if __name__ == "__main__":
    unittest.main()
