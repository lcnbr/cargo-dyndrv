"""Seal declared, completed evidence into a reproducible, self-validating ZIP.

The producer owns scientific validation and the meaning of its completion gates.
This command only binds retained bytes and selected JSON fields; it runs no work.
"""

import argparse
import hashlib
import json
import os
import re
import stat
import sys
import tempfile
import time
import zipfile
from datetime import UTC, datetime
from pathlib import Path, PurePosixPath


class EvidencePacket:
    VERSION = 1
    RECORDS = frozenset({"source", "run", "compiler", "tests", "timing"})
    CHUNK = 1024 * 1024

    @staticmethod
    def encode(value):
        return (
            json.dumps(value, sort_keys=True, indent=2, allow_nan=False) + "\n"
        ).encode()

    @staticmethod
    def decode(contents):
        def unique(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError(f"duplicate JSON key: {key}")
                result[key] = value
            return result

        def invalid(value):
            raise ValueError(f"non-finite JSON number: {value}")

        value = json.loads(contents, object_pairs_hook=unique, parse_constant=invalid)
        # This also rejects overflowing numeric literals, such as 1e999.
        EvidencePacket.encode(value)
        return value

    @staticmethod
    def fields(value, expected, label):
        if not isinstance(value, dict) or value.keys() != expected:
            raise ValueError(f"{label} requires exactly: {', '.join(sorted(expected))}")

    @staticmethod
    def relative(name):
        if (
            not isinstance(name, str)
            or not name
            or "\0" in name
            or "\\" in name
            or PurePosixPath(name).is_absolute()
            or any(part in {"", ".", ".."} for part in name.split("/"))
        ):
            raise ValueError(f"expected a canonical relative file path: {name!r}")
        return name

    @classmethod
    def specification(cls, value):
        cls.fields(
            value, {"version", "scope", "files", "records", "checks"}, "specification"
        )
        if type(value["version"]) is not int or value["version"] != cls.VERSION:
            raise ValueError("unsupported evidence specification version")
        if not isinstance(value["scope"], str) or not value["scope"].strip():
            raise ValueError("scope must describe the declared evidence boundary")
        files = value["files"]
        if not isinstance(files, list) or not files:
            raise ValueError("files must be a nonempty explicit list")
        files = [cls.relative(name) for name in files]
        if len(files) != len(set(files)):
            raise ValueError("duplicate evidence file")
        names = set(files)
        for name in names:
            if any(str(parent) in names for parent in PurePosixPath(name).parents):
                raise ValueError(f"file/directory path conflict: {name}")

        def binding(item, check=False):
            cls.fields(
                item,
                {"file", "pointer", "equals"} if check else {"file", "pointer"},
                "completion check" if check else "record binding",
            )
            if cls.relative(item["file"]) not in names:
                raise ValueError(f"binding file is not declared: {item['file']}")
            pointer = item["pointer"]
            if not isinstance(pointer, str) or (
                pointer and not pointer.startswith("/")
            ):
                raise ValueError("pointer must be empty or start with '/'")
            if re.search(r"~(?![01])", pointer):
                raise ValueError(f"invalid JSON pointer escape: {pointer}")

        cls.fields(value["records"], cls.RECORDS, "records")
        for group, records in value["records"].items():
            if not isinstance(records, dict) or not records:
                raise ValueError(f"{group} must contain at least one record binding")
            for label, item in records.items():
                if not isinstance(label, str) or not label.strip():
                    raise ValueError("record labels must be nonempty strings")
                binding(item)
        if not isinstance(value["checks"], list) or not value["checks"]:
            raise ValueError("at least one producer completion check is required")
        for item in value["checks"]:
            binding(item, check=True)
        return {
            **value,
            "files": sorted(files),
            "checks": sorted(value["checks"], key=cls.encode),
        }

    @staticmethod
    def select(document, pointer):
        if not pointer:
            return document
        for token in pointer[1:].split("/"):
            token = token.replace("~1", "/").replace("~0", "~")
            if isinstance(document, dict) and token in document:
                document = document[token]
            elif (
                isinstance(document, list)
                and re.fullmatch(r"0|[1-9][0-9]*", token)
                and int(token) < len(document)
            ):
                document = document[int(token)]
            else:
                raise ValueError(f"missing JSON pointer: {pointer}")
        return document

    @classmethod
    def records(cls, specification, archive):
        documents = {}

        def selected(binding):
            name = binding["file"]
            if name not in documents:
                documents[name] = cls.decode(archive.read("files/" + name))
            return cls.select(documents[name], binding["pointer"])

        for check in specification["checks"]:
            # JSON equality is type-sensitive: true must not satisfy an exit code 1.
            if cls.encode(selected(check)) != cls.encode(check["equals"]):
                raise ValueError(
                    f"completion check failed: {check['file']}#{check['pointer']}"
                )
        result = {}
        for group, bindings in specification["records"].items():
            result[group] = {}
            for label, binding in bindings.items():
                result[group][label] = selected(binding)
        return result

    @staticmethod
    def fingerprint(info):
        return (
            info.st_dev,
            info.st_ino,
            info.st_mode,
            info.st_size,
            info.st_mtime_ns,
            info.st_ctime_ns,
        )

    @classmethod
    def source(cls, root, name):
        path = root
        parts = PurePosixPath(name).parts
        for index, part in enumerate(parts):
            path = path / part
            info = path.lstat()
            expected = stat.S_ISREG if index == len(parts) - 1 else stat.S_ISDIR
            if not expected(info.st_mode):
                raise ValueError(f"evidence must be regular files/directories: {name}")
        return path, cls.fingerprint(info)

    @classmethod
    def transfer(cls, source, destination=None):
        digest = hashlib.sha256()
        size = 0
        while chunk := source.read(cls.CHUNK):
            digest.update(chunk)
            size += len(chunk)
            if destination is not None:
                destination.write(chunk)
        return {"size": size, "sha256": digest.hexdigest()}

    @staticmethod
    def entry(name):
        entry = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
        entry.create_system = 3
        entry.external_attr = (stat.S_IFREG | 0o644) << 16
        return entry

    @classmethod
    def validate(cls, packet):
        packet = Path(packet)
        with packet.open("rb") as packet_stream:
            before = cls.fingerprint(os.fstat(packet_stream.fileno()))
            with zipfile.ZipFile(packet_stream) as archive:
                manifest_bytes = archive.read("manifest.json")
                manifest = cls.decode(manifest_bytes)
                cls.fields(
                    manifest,
                    {"version", "specification", "records", "files"},
                    "manifest",
                )
                if (
                    type(manifest["version"]) is not int
                    or manifest["version"] != cls.VERSION
                ):
                    raise ValueError("unsupported evidence manifest version")
                specification = cls.specification(manifest["specification"])
                expected = ["files/" + name for name in specification["files"]]
                expected.append("manifest.json")
                entries = archive.infolist()
                if [entry.filename for entry in entries] != expected:
                    raise ValueError(
                        "archive inventory/order differs from declared files"
                    )
                for entry in entries:
                    if (
                        entry.create_system != 3
                        or entry.external_attr >> 16 != stat.S_IFREG | 0o644
                        or entry.compress_type != zipfile.ZIP_STORED
                        or entry.date_time != (1980, 1, 1, 0, 0, 0)
                        or entry.flag_bits & 1
                    ):
                        raise ValueError(
                            f"unexpected archive entry metadata: {entry.filename}"
                        )
                inventory = []
                for name in specification["files"]:
                    with archive.open("files/" + name) as contents:
                        inventory.append({"path": name, **cls.transfer(contents)})
                actual = {
                    "version": cls.VERSION,
                    "specification": specification,
                    "records": cls.records(specification, archive),
                    "files": inventory,
                }
                if manifest_bytes != cls.encode(actual):
                    raise ValueError(
                        "manifest contents or file hashes do not match the packet"
                    )
            packet_stream.seek(0)
            identity = cls.transfer(packet_stream)
            if (
                cls.fingerprint(os.fstat(packet_stream.fileno())) != before
                or cls.fingerprint(packet.stat()) != before
            ):
                raise ValueError("packet changed during validation")
        return {
            "packet_sha256": identity["sha256"],
            "packet_bytes": identity["size"],
            "manifest_sha256": hashlib.sha256(manifest_bytes).hexdigest(),
            "files": len(inventory),
            "payload_bytes": sum(item["size"] for item in inventory),
        }

    @classmethod
    def package(cls, specification, root, output):
        specification = cls.specification(specification)
        root = Path(root).resolve(strict=True)
        output = Path(output).absolute()
        if os.path.lexists(output):
            raise FileExistsError(f"output already exists: {output}")
        inventory = []
        originals = {}
        started = time.perf_counter()
        with tempfile.NamedTemporaryFile(
            prefix=f".{output.name}.", suffix=".partial", dir=output.parent
        ) as temporary:
            with zipfile.ZipFile(temporary, "w", allowZip64=True) as archive:
                for name in specification["files"]:
                    source, before = cls.source(root, name)
                    with source.open("rb") as contents:
                        if cls.fingerprint(os.fstat(contents.fileno())) != before:
                            raise ValueError(f"evidence changed before reading: {name}")
                        with archive.open(
                            cls.entry("files/" + name), "w", force_zip64=True
                        ) as destination:
                            identity = cls.transfer(contents, destination)
                        if cls.fingerprint(os.fstat(contents.fileno())) != before:
                            raise ValueError(f"evidence changed while reading: {name}")
                    originals[name] = before
                    inventory.append({"path": name, **identity})
                manifest = {
                    "version": cls.VERSION,
                    "specification": specification,
                    "records": cls.records(specification, archive),
                    "files": inventory,
                }
                archive.writestr(cls.entry("manifest.json"), cls.encode(manifest))
            temporary.flush()
            os.fsync(temporary.fileno())
            copy_seconds = time.perf_counter() - started
            checking = time.perf_counter()
            result = cls.validate(temporary.name)
            for name, before in originals.items():
                if cls.source(root, name)[1] != before:
                    raise ValueError(f"evidence changed during packaging: {name}")
            validation_seconds = time.perf_counter() - checking
            # Linking a fully written, validated sibling is atomic and cannot replace
            # an existing destination, including a concurrent successful publisher.
            os.link(temporary.name, output)
        return {
            **result,
            "copy_seconds": copy_seconds,
            "validation_seconds": validation_seconds,
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    package = commands.add_parser("package", help="seal existing evidence files")
    package.add_argument("--spec", type=Path, required=True)
    package.add_argument("--root", type=Path, required=True)
    package.add_argument("--output", type=Path, required=True)
    validate = commands.add_parser("validate", help="verify a self-contained packet")
    validate.add_argument("packet", type=Path)
    args = parser.parse_args()
    started = time.perf_counter()
    cpu = time.process_time()
    started_utc = datetime.now(UTC).isoformat()
    try:
        if args.command == "package":
            result = EvidencePacket.package(
                EvidencePacket.decode(args.spec.read_bytes()), args.root, args.output
            )
        else:
            result = EvidencePacket.validate(args.packet)
    except (OSError, ValueError, KeyError, zipfile.BadZipFile, RuntimeError) as error:
        print(f"evidence: {error}", file=sys.stderr)
        return 1
    print(
        EvidencePacket.encode(
            {
                "version": 1,
                "operation": args.command,
                "validation": "declared-files-and-checks",
                **result,
                "started_utc": started_utc,
                "finished_utc": datetime.now(UTC).isoformat(),
                "wall_seconds": time.perf_counter() - started,
                "cpu_seconds": time.process_time() - cpu,
            }
        ).decode(),
        end="",
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
