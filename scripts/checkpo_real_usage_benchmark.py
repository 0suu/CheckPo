#!/usr/bin/env python3
"""Reproducible real-usage benchmark driver for CheckPo.

The driver intentionally invokes the release CLI as a fresh process for every
operation. It writes only below the supplied benchmark root.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Any


TIME_PATTERN = re.compile(r"^(real|user|sys)\s+([0-9.]+)$", re.MULTILINE)
RSS_PATTERN = re.compile(r"^\s*([0-9]+)\s+maximum resident set size$", re.MULTILINE)


def allocated_bytes(path: Path) -> int:
    stat = path.stat()
    if os.name != "nt":
        return stat.st_blocks * 512
    get_compressed_file_size = ctypes.WinDLL(
        "kernel32", use_last_error=True
    ).GetCompressedFileSizeW
    get_compressed_file_size.argtypes = [
        ctypes.c_wchar_p,
        ctypes.POINTER(ctypes.c_ulong),
    ]
    get_compressed_file_size.restype = ctypes.c_ulong
    high = ctypes.c_ulong(0)
    ctypes.set_last_error(0)
    low = get_compressed_file_size(str(path), ctypes.byref(high))
    error = ctypes.get_last_error()
    if low == 0xFFFFFFFF and error:
        raise OSError(error, f"GetCompressedFileSizeW failed for {path}")
    return (high.value << 32) | low


def filesystem_metadata(path: Path, repository: Path) -> Any:
    if os.name != "nt":
        mount_point = run_text(["df", "-P", str(path)]).splitlines()[-1].split()[-1]
        return run_text(["diskutil", "info", mount_point], repository)

    root = f"{path.resolve().drive}\\"
    volume_name = ctypes.create_unicode_buffer(261)
    filesystem_name = ctypes.create_unicode_buffer(261)
    serial_number = ctypes.c_ulong()
    maximum_component_length = ctypes.c_ulong()
    flags = ctypes.c_ulong()
    if not ctypes.windll.kernel32.GetVolumeInformationW(
        root,
        volume_name,
        len(volume_name),
        ctypes.byref(serial_number),
        ctypes.byref(maximum_component_length),
        ctypes.byref(flags),
        filesystem_name,
        len(filesystem_name),
    ):
        raise ctypes.WinError()
    usage = shutil.disk_usage(path)
    return {
        "root": root,
        "volumeName": volume_name.value,
        "filesystemName": filesystem_name.value,
        "volumeSerialNumber": serial_number.value,
        "maximumComponentLength": maximum_component_length.value,
        "flags": flags.value,
        "totalBytes": usage.total,
        "freeBytes": usage.free,
    }


def copy_tracked_tree(source: Path, destination: Path) -> None:
    if os.name != "nt":
        subprocess.run(["cp", "-cR", str(source), str(destination)], check=True)
        return
    completed = subprocess.run(
        [
            "robocopy",
            str(source),
            str(destination),
            "/E",
            "/COPY:DAT",
            "/DCOPY:DAT",
            "/R:1",
            "/W:1",
            "/XJ",
            "/NFL",
            "/NDL",
            "/NJH",
            "/NJS",
            "/NP",
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if completed.returncode >= 8:
        raise RuntimeError(
            f"robocopy failed with {completed.returncode}: {completed.stderr.strip()}"
        )


def sha256_path(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run_text(command: list[str], cwd: Path | None = None) -> str:
    return subprocess.run(
        command,
        cwd=cwd,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ).stdout.strip()


class Benchmark:
    def __init__(self, root: Path, cli: Path) -> None:
        self.root = root
        self.project = root / "UnityProject"
        self.data = root / "CheckPoData"
        self.results = root / "results"
        self.cli = cli.resolve()
        self.run_id = uuid.uuid4().hex
        self.env = os.environ.copy()
        self.env["CHECKPO_DATA_DIR"] = str(self.data)
        self.results.mkdir(parents=True, exist_ok=True)
        environment_path = self.results / "environment.json"
        if environment_path.is_file():
            metadata = json.loads(environment_path.read_text(encoding="utf-8"))
            expected_cli = metadata.get("cliSha256")
            actual_cli = sha256_path(self.cli)
            if expected_cli != actual_cli:
                raise RuntimeError(
                    f"CLI binary differs from immutable benchmark environment: "
                    f"expected {expected_cli}, got {actual_cli}"
                )
            expected_script = metadata.get("benchmarkScriptSha256")
            actual_script = sha256_path(Path(__file__).resolve())
            if expected_script != actual_script:
                raise RuntimeError(
                    "benchmark script differs from immutable benchmark environment: "
                    f"expected {expected_script}, got {actual_script}"
                )

    def record(self, name: str, value: dict[str, Any]) -> None:
        destination = self.results / f"{name}.jsonl"
        with destination.open("a", encoding="utf-8") as output:
            output.write(json.dumps(value, ensure_ascii=False, separators=(",", ":")))
            output.write("\n")
            output.flush()

    def cli_json(self, label: str, arguments: list[str]) -> dict[str, Any]:
        if os.name == "nt":
            command = [str(self.cli), "--json", *arguments]
        else:
            command = ["/usr/bin/time", "-lp", str(self.cli), "--json", *arguments]
        started = time.perf_counter_ns()
        completed = subprocess.run(
            command,
            env=self.env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        external_micros = (time.perf_counter_ns() - started) // 1_000
        timing = (
            {}
            if os.name == "nt"
            else {key: float(value) for key, value in TIME_PATTERN.findall(completed.stderr)}
        )
        rss = None if os.name == "nt" else RSS_PATTERN.search(completed.stderr)
        event: dict[str, Any] = {
            "runId": self.run_id,
            "label": label,
            "utcEpochSeconds": time.time(),
            "command": command,
            "exitCode": completed.returncode,
            "externalMicros": external_micros,
            "processTimingSeconds": timing,
            "maximumResidentSetBytes": int(rss.group(1)) if rss else None,
            "stderr": completed.stderr,
        }
        if completed.stdout.strip():
            try:
                event["result"] = json.loads(completed.stdout)
            except json.JSONDecodeError:
                event["stdout"] = completed.stdout
        self.record("operations", event)
        if completed.returncode != 0:
            raise RuntimeError(
                f"{label} failed with {completed.returncode}: {completed.stderr.strip()}"
            )
        return event

    def checkpoint_count(self) -> int:
        event = self.cli_json(
            "checkpoint-list-before-loop", ["checkpoint", "list", str(self.project)]
        )
        return len(event.get("result") or [])

    def checkpoint_list(self, label: str) -> list[dict[str, Any]]:
        event = self.cli_json(label, ["checkpoint", "list", str(self.project)])
        return event.get("result") or []

    def local_action(self, label: str, action: Any) -> dict[str, Any]:
        started = time.perf_counter_ns()
        details = action() or {}
        event = {
            "runId": self.run_id,
            "label": label,
            "utcEpochSeconds": time.time(),
            "externalMicros": (time.perf_counter_ns() - started) // 1_000,
            "details": details,
        }
        self.record("scenario-actions", event)
        return event

    def save_plan(self, label: str, plan: dict[str, Any]) -> Path:
        destination = self.results / f"{label}-{self.run_id}-expected-plan.json"
        temporary = destination.with_suffix(".tmp")
        temporary.write_text(
            json.dumps(plan, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        os.replace(temporary, destination)
        return destination

    def begin_scenario(self, name: str, baseline: str) -> Path:
        state_directory = self.results / f"{name}-state"
        try:
            state_directory.mkdir()
        except FileExistsError:
            destination = state_directory / "state.json"
            state = (
                json.loads(destination.read_text(encoding="utf-8"))
                if destination.is_file()
                else {}
            )
            raise RuntimeError(
                f"scenario {name} already started with state {state.get('status')}; "
                "use a fresh benchmark root instead of mixing retries"
            )
        destination = state_directory / "state.json"
        temporary = destination.with_suffix(".tmp")
        temporary.write_text(
            json.dumps(
                {
                    "scenario": name,
                    "runId": self.run_id,
                    "baselineCheckpointId": baseline,
                    "status": "running",
                    "startedAtEpochSeconds": time.time(),
                },
                ensure_ascii=False,
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        os.replace(temporary, destination)
        return destination

    def complete_scenario(self, state_path: Path) -> None:
        state = json.loads(state_path.read_text(encoding="utf-8"))
        state["status"] = "complete"
        state["completedAtEpochSeconds"] = time.time()
        temporary = state_path.with_suffix(".tmp")
        temporary.write_text(
            json.dumps(state, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        os.replace(temporary, state_path)


def command_prepare(args: argparse.Namespace) -> None:
    root = args.root.resolve()
    project = root / "UnityProject"
    tracked_roots = ("Assets", "Packages", "ProjectSettings")
    prepared = root.is_dir() and all((project / name).is_dir() for name in tracked_roots)
    if root.exists() and not prepared:
        raise SystemExit(f"benchmark root exists but is incomplete: {root}")
    environment_path = root / "results" / "environment.json"
    if prepared:
        if not environment_path.is_file():
            raise SystemExit(
                f"prepared benchmark has no immutable environment record: {environment_path}"
            )
        metadata = json.loads(environment_path.read_text(encoding="utf-8"))
        actual_cli_sha256 = sha256_path(args.cli.resolve())
        if metadata.get("cliSha256") != actual_cli_sha256:
            raise SystemExit(
                "prepared benchmark was created with a different CLI binary: "
                f"expected {metadata.get('cliSha256')}, got {actual_cli_sha256}"
            )
        actual_script_sha256 = sha256_path(Path(__file__).resolve())
        if metadata.get("benchmarkScriptSha256") != actual_script_sha256:
            raise SystemExit(
                "prepared benchmark was created with a different benchmark script: "
                f"expected {metadata.get('benchmarkScriptSha256')}, "
                f"got {actual_script_sha256}"
            )
        print(json.dumps(metadata, ensure_ascii=False, indent=2))
        return
    if not prepared:
        project.mkdir(parents=True)
        for tracked_root in tracked_roots:
            source = args.source.resolve() / tracked_root
            if not source.is_dir():
                raise SystemExit(f"missing tracked root: {source}")
            copy_tracked_tree(source, project / tracked_root)
    sentinel = project / "Assets" / "CheckPoBenchmark" / "Rotation.asset"
    sentinel.parent.mkdir(parents=True, exist_ok=True)
    sentinel.write_text("checkpoint-000000\n", encoding="utf-8")
    bench = Benchmark(root, args.cli)
    metadata = {
        "createdAtEpochSeconds": time.time(),
        "root": str(root),
        "source": str(args.source.resolve()),
        "project": str(project),
        "cli": str(bench.cli),
        "cliSha256": sha256_path(bench.cli),
        "benchmarkScriptSha256": sha256_path(Path(__file__).resolve()),
        "python": sys.version,
        "platform": platform.platform(),
        "machine": platform.machine(),
        "gitHead": run_text(["git", "rev-parse", "HEAD"], args.repository),
        "gitDirty": bool(run_text(["git", "status", "--porcelain"], args.repository)),
        "filesystem": filesystem_metadata(root, args.repository),
        "rayonThreads": os.environ.get("RAYON_NUM_THREADS"),
        "objectWriteConcurrency": os.environ.get(
            "CHECKPO_OBJECT_WRITE_CONCURRENCY"
        ),
        "manifestWriteConcurrency": os.environ.get(
            "CHECKPO_MANIFEST_WRITE_CONCURRENCY"
        ),
        "transactionStageConcurrency": os.environ.get(
            "CHECKPO_TRANSACTION_STAGE_CONCURRENCY"
        ),
    }
    environment_path.write_text(
        json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(metadata, ensure_ascii=False, indent=2))


def command_checkpoints(args: argparse.Namespace) -> None:
    bench = Benchmark(args.root.resolve(), args.cli)
    if not (bench.project / ".checkpo" / "project.json").exists():
        bench.cli_json("project-init", ["init", str(bench.project)])
    existing_checkpoints = bench.checkpoint_list("checkpoint-list-before-loop")
    current = len(existing_checkpoints)
    series_path = bench.results / "checkpoint-series.jsonl"
    recorded_ids: set[str] = set()
    if series_path.is_file():
        with series_path.open(encoding="utf-8") as source:
            recorded_ids = {
                event.get("checkpointId")
                for line in source
                if (event := json.loads(line)).get("checkpointId")
            }
    missing_measurements = [
        checkpoint["checkpointId"]
        for checkpoint in existing_checkpoints
        if checkpoint.get("name", "").startswith("bench-cli-")
        and checkpoint["checkpointId"] not in recorded_ids
    ]
    if missing_measurements:
        raise RuntimeError(
            "checkpoint creation succeeded without a matching measurement record; "
            f"refusing to hide the gap: {missing_measurements}"
        )
    if current > args.to:
        raise SystemExit(f"repository already has {current} checkpoints, target is {args.to}")
    sentinel = bench.project / "Assets" / "CheckPoBenchmark" / "Rotation.asset"
    for checkpoint_number in range(current + 1, args.to + 1):
        sentinel.write_text(f"checkpoint-{checkpoint_number:06}\n", encoding="utf-8")
        event = bench.cli_json(
            f"checkpoint-{checkpoint_number:06}",
            [
                "checkpoint",
                "create",
                str(bench.project),
                "--name",
                f"bench-cli-{checkpoint_number:06}",
                "--timings",
            ],
        )
        result = event.get("result") or {}
        summary = {
            "runId": bench.run_id,
            "source": "cli",
            "checkpointNumber": checkpoint_number,
            "checkpointId": result.get("checkpointId"),
            "externalMicros": event["externalMicros"],
            "maximumResidentSetBytes": event["maximumResidentSetBytes"],
            "createMetrics": result.get("createMetrics"),
        }
        bench.record("checkpoint-series", summary)
        if checkpoint_number == 1 or checkpoint_number % 10 == 0:
            print(
                f"checkpoint {checkpoint_number}/{args.to}: "
                f"{event['externalMicros'] / 1_000_000:.3f}s",
                flush=True,
            )
    recorded = [
        json.loads(line)
        for line in series_path.read_text(encoding="utf-8").splitlines()
        if line
    ]
    numbers = [event.get("checkpointNumber") for event in recorded]
    ids = [event.get("checkpointId") for event in recorded]
    if numbers != list(range(1, args.to + 1)):
        raise RuntimeError("checkpoint series is not exactly continuous from 1 to target")
    if None in ids or len(ids) != len(set(ids)):
        raise RuntimeError("checkpoint series contains a missing or duplicate checkpoint ID")


def latest_checkpoint(bench: Benchmark) -> str:
    event = bench.cli_json("checkpoint-list", ["checkpoint", "list", str(bench.project)])
    checkpoints = event.get("result") or []
    if not checkpoints:
        raise RuntimeError("checkpoint list is empty")
    return checkpoints[0]["checkpointId"]


def command_operations(args: argparse.Namespace) -> None:
    bench = Benchmark(args.root.resolve(), args.cli)
    checkpoints = bench.checkpoint_list(f"milestone-{args.milestone}-checkpoint-list")
    if len(checkpoints) != args.expected_checkpoint_count:
        raise RuntimeError(
            f"milestone {args.milestone} expected {args.expected_checkpoint_count} "
            f"checkpoints, found {len(checkpoints)}"
        )
    checkpoint = checkpoints[0]["checkpointId"]
    if checkpoints[0].get("name") != args.expected_latest_name:
        raise RuntimeError(
            f"milestone {args.milestone} expected latest name "
            f"{args.expected_latest_name}, found {checkpoints[0].get('name')}"
        )
    series_path = bench.results / "checkpoint-series.jsonl"
    if args.milestone <= 1000 and series_path.is_file():
        series = [
            json.loads(line)
            for line in series_path.read_text(encoding="utf-8").splitlines()
            if line
        ]
        matching = [
            event for event in series if event.get("checkpointNumber") == args.milestone
        ]
        if len(matching) != 1 or matching[0].get("checkpointId") != checkpoint:
            raise RuntimeError("milestone latest checkpoint does not match its series record")
    operations = [
        ("status", ["status", str(bench.project)]),
        ("diff-latest", ["diff", str(bench.project), "--checkpoint", checkpoint]),
        ("verify-quick", ["verify", str(bench.project), "--quick"]),
        ("index-rebuild", ["index", "rebuild", str(bench.project)]),
        ("status-after-rebuild", ["status", str(bench.project)]),
        ("fresh-process-status-1", ["status", str(bench.project)]),
        ("fresh-process-status-2", ["status", str(bench.project)]),
    ]
    operation_events: dict[str, dict[str, Any]] = {}
    for label, command in operations:
        event = bench.cli_json(f"milestone-{args.milestone}-{label}", command)
        operation_events[label] = event
        print(f"{label}: {event['externalMicros'] / 1_000_000:.3f}s", flush=True)
    status = operation_events["status"].get("result") or {}
    if (
        (status.get("storage") or {}).get("checkpointCount")
        != args.expected_checkpoint_count
        or (status.get("checkpointIndex") or {}).get("state") != "current"
        or status.get("pendingTransactions")
        or status.get("unresolvedQuarantines")
        or status.get("warnings")
    ):
        raise RuntimeError(f"milestone status is not clean/current: {status}")
    assert_diff_counts(
        operation_events["diff-latest"], added=0, modified=0, deleted=0
    )
    quick = operation_events["verify-quick"].get("result") or {}
    if not quick.get("isValid") or quick.get("errors") or quick.get("warnings"):
        raise RuntimeError(f"milestone quick verification failed: {quick}")
    rebuild = operation_events["index-rebuild"].get("result") or {}
    if (
        rebuild.get("snapshotCount") != args.expected_checkpoint_count
        or rebuild.get("unavailableReferencedObjectCount") != 0
        or rebuild.get("errors")
    ):
        raise RuntimeError(f"milestone index rebuild is incomplete: {rebuild}")
    gc_before = bench.cli_json(
        f"milestone-{args.milestone}-gc-analyze-before",
        ["storage", "gc", "analyze", str(bench.project)],
    )
    gc_before_result = gc_before.get("result") or {}
    if (
        gc_before_result.get("checkpointCount") != args.expected_checkpoint_count
        or gc_before_result.get("hasIntegrityProblems")
    ):
        raise RuntimeError(f"milestone GC analysis found an integrity problem: {gc_before_result}")
    print(
        f"gc-analyze-before: {gc_before['externalMicros'] / 1_000_000:.3f}s",
        flush=True,
    )
    gc_plan_path = bench.save_plan(
        f"milestone-{args.milestone}-gc", gc_before.get("result") or {}
    )
    gc_apply = bench.cli_json(
        f"milestone-{args.milestone}-gc-apply",
        [
            "storage",
            "gc",
            "apply",
            str(bench.project),
            "--expected-plan",
            str(gc_plan_path),
            "--yes",
        ],
    )
    if not (gc_apply.get("result") or {}).get("applied"):
        raise RuntimeError("milestone GC apply did not report applied=true")
    print(
        f"gc-apply: {gc_apply['externalMicros'] / 1_000_000:.3f}s", flush=True
    )
    gc_after = bench.cli_json(
        f"milestone-{args.milestone}-gc-analyze-after",
        ["storage", "gc", "analyze", str(bench.project)],
    )
    gc_after_result = gc_after.get("result") or {}
    if (
        gc_after_result.get("hasIntegrityProblems")
        or gc_after_result.get("unreferencedBlobCount") != 0
        or gc_after_result.get("unreferencedManifestChunkCount") != 0
    ):
        raise RuntimeError(f"milestone GC did not leave a clean repository: {gc_after_result}")
    print(
        f"gc-analyze-after: {gc_after['externalMicros'] / 1_000_000:.3f}s",
        flush=True,
    )
    post_gc_quick = bench.cli_json(
        f"milestone-{args.milestone}-verify-quick-after-gc",
        ["verify", str(bench.project), "--quick"],
    )
    post_gc_result = post_gc_quick.get("result") or {}
    if (
        not post_gc_result.get("isValid")
        or post_gc_result.get("errors")
        or post_gc_result.get("warnings")
    ):
        raise RuntimeError(f"post-GC quick verification failed: {post_gc_result}")
    print(
        f"verify-quick-after-gc: {post_gc_quick['externalMicros'] / 1_000_000:.3f}s",
        flush=True,
    )
    if args.full_verify:
        event = bench.cli_json(
            f"milestone-{args.milestone}-verify-full-after-gc",
            ["verify", str(bench.project)],
        )
        result = event.get("result") or {}
        if not result.get("isValid") or result.get("errors") or result.get("warnings"):
            raise RuntimeError(f"milestone full verification failed: {result}")
        print(f"verify-full-after-gc: {event['externalMicros'] / 1_000_000:.3f}s", flush=True)


def create_named_checkpoint(bench: Benchmark, label: str, name: str) -> dict[str, Any]:
    return bench.cli_json(
        label,
        [
            "checkpoint",
            "create",
            str(bench.project),
            "--name",
            name,
            "--timings",
        ],
    )


def assert_diff_counts(
    event: dict[str, Any], *, added: int, modified: int, deleted: int
) -> None:
    result = event.get("result") or {}
    actual = (len(result.get("added") or []), len(result.get("modified") or []), len(result.get("deleted") or []))
    expected = (added, modified, deleted)
    if actual != expected:
        raise RuntimeError(f"unexpected diff counts: expected {expected}, got {actual}")


def apply_restore(bench: Benchmark, label: str, checkpoint: str) -> dict[str, Any]:
    preview = bench.cli_json(
        f"{label}-preview",
        ["restore", "preview", str(bench.project), "--checkpoint", checkpoint],
    )
    plan = preview.get("result") or {}
    plan_path = bench.save_plan(label, plan)
    applied = bench.cli_json(
        f"{label}-apply",
        [
            "restore",
            "apply",
            str(bench.project),
            "--checkpoint",
            checkpoint,
            "--expected-plan",
            str(plan_path),
            "--yes",
        ],
    )
    return applied


def apply_discard(
    bench: Benchmark, label: str, checkpoint: str, selected_path: str
) -> dict[str, Any]:
    preview = bench.cli_json(
        f"{label}-preview",
        [
            "discard",
            "preview",
            str(bench.project),
            "--path",
            selected_path,
            "--checkpoint",
            checkpoint,
        ],
    )
    plan = preview.get("result") or {}
    plan_path = bench.save_plan(label, plan)
    applied = bench.cli_json(
        f"{label}-apply",
        [
            "discard",
            "apply",
            str(bench.project),
            "--path",
            selected_path,
            "--checkpoint",
            checkpoint,
            "--expected-plan",
            str(plan_path),
            "--yes",
        ],
    )
    return applied


def small_tree_details(root: Path) -> dict[str, Any]:
    files = [path for path in root.rglob("*") if path.is_file()] if root.exists() else []
    return {
        "fileCount": len(files),
        "logicalBytes": sum(path.stat().st_size for path in files),
    }


def generate_small_tree(root: Path) -> dict[str, Any]:
    if root.exists():
        raise RuntimeError(f"small-file tree already exists: {root}")
    for directory_number in range(100):
        directory = root / f"Set{directory_number:03}"
        directory.mkdir(parents=True)
        for file_number in range(100):
            stem = f"Item{file_number:03}"
            unique = directory_number * 100 + file_number
            (directory / f"{stem}.asset").write_text(
                f"%YAML 1.1\n--- !u!114 &{unique + 1}\nvalue: benchmark-{unique:05}\n",
                encoding="utf-8",
            )
            (directory / f"{stem}.asset.meta").write_text(
                f"fileFormatVersion: 2\nguid: {unique:032x}\n",
                encoding="utf-8",
            )
    details = small_tree_details(root)
    if details["fileCount"] != 20_000:
        raise RuntimeError(f"expected 20000 small files, got {details['fileCount']}")
    return details


def command_small_files(args: argparse.Namespace) -> None:
    bench = Benchmark(args.root.resolve(), args.cli)
    baseline = bench.checkpoint_list("small-files-baseline-list")[0]["checkpointId"]
    selected = "Assets/CheckPoBenchmark/SmallFiles"
    tree = bench.project / selected
    if tree.exists():
        raise SystemExit(f"small-file scenario path already exists: {tree}")
    clean = bench.cli_json(
        "small-files-baseline-clean-diff",
        ["diff", str(bench.project), "--checkpoint", baseline],
    )
    assert_diff_counts(clean, added=0, modified=0, deleted=0)
    scenario_state = bench.begin_scenario("small-files-20000", baseline)

    added = bench.local_action(
        "small-files-add-for-discard", lambda: generate_small_tree(tree)
    )
    diff = bench.cli_json(
        "small-files-diff-added-for-discard",
        ["diff", str(bench.project), "--checkpoint", baseline],
    )
    assert_diff_counts(diff, added=20_000, modified=0, deleted=0)
    discarded = apply_restore(bench, "small-files-delete-added-via-baseline", baseline)
    if tree.exists():
        remaining = small_tree_details(tree)
        if remaining["fileCount"]:
            raise RuntimeError(f"baseline restore left {remaining['fileCount']} small files")
        bench.local_action(
            "small-files-remove-empty-directory-after-restore",
            lambda: (shutil.rmtree(tree), {"removedEmptyTree": True})[1],
        )
    discarded_clean = bench.cli_json(
        "small-files-diff-after-delete-added",
        ["diff", str(bench.project), "--checkpoint", baseline],
    )
    assert_diff_counts(discarded_clean, added=0, modified=0, deleted=0)

    tree_generation = bench.local_action(
        "small-files-add-for-checkpoint", lambda: generate_small_tree(tree)
    )
    present = create_named_checkpoint(
        bench, "small-files-checkpoint-present", "bench-small-files-20000"
    )
    present_id = (present.get("result") or {})["checkpointId"]
    removed = bench.local_action(
        "small-files-delete-working-tree",
        lambda: (shutil.rmtree(tree), {"removedFileCount": 20_000})[1],
    )
    diff = bench.cli_json(
        "small-files-diff-deleted",
        ["diff", str(bench.project), "--checkpoint", present_id],
    )
    assert_diff_counts(diff, added=0, modified=0, deleted=20_000)
    restored = apply_restore(bench, "small-files-restore-deleted", present_id)
    restored_details = small_tree_details(tree)
    if restored_details["fileCount"] != 20_000:
        raise RuntimeError(f"restore produced {restored_details['fileCount']} small files")
    clean_diff = bench.cli_json(
        "small-files-diff-after-restore",
        ["diff", str(bench.project), "--checkpoint", present_id],
    )
    assert_diff_counts(clean_diff, added=0, modified=0, deleted=0)
    apply_restore(bench, "small-files-restore-baseline-cleanup", baseline)
    if tree.exists():
        remaining = small_tree_details(tree)
        if remaining["fileCount"]:
            raise RuntimeError(
                f"baseline cleanup left {remaining['fileCount']} small files"
            )
        bench.local_action(
            "small-files-remove-empty-directory-after-cleanup",
            lambda: (shutil.rmtree(tree), {"removedEmptyTree": True})[1],
        )
    cleanup_diff = bench.cli_json(
        "small-files-diff-after-baseline-cleanup",
        ["diff", str(bench.project), "--checkpoint", baseline],
    )
    assert_diff_counts(cleanup_diff, added=0, modified=0, deleted=0)
    verify = bench.cli_json("small-files-verify-quick", ["verify", str(bench.project), "--quick"])
    bench.record(
        "scenario-summary",
        {
            "runId": bench.run_id,
            "scenario": "small-files-20000",
            "baselineCheckpointId": baseline,
            "presentCheckpointId": present_id,
            "addMicros": added["externalMicros"],
            "treeGenerationMicros": tree_generation["externalMicros"],
            "checkpointCreateMicros": present["externalMicros"],
            "deleteMicros": removed["externalMicros"],
            "restoredDetails": restored_details,
            "discardResult": discarded.get("result"),
            "restoreResult": restored.get("result"),
            "verifyResult": verify.get("result"),
        },
    )
    bench.complete_scenario(scenario_state)
    print("small-files-20000: complete", flush=True)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def large_file_details(path: Path) -> dict[str, Any]:
    stat = path.stat()
    return {
        "logicalBytes": stat.st_size,
        "allocatedBytes": allocated_bytes(path),
        "sha256": sha256_file(path),
    }


def generate_large_file(path: Path) -> dict[str, Any]:
    if path.exists():
        raise RuntimeError(f"large benchmark file already exists: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    block = b"".join(hashlib.sha256(f"checkpo-{index}".encode()).digest() for index in range(32_768))
    if len(block) != 1024 * 1024:
        raise RuntimeError("large-file generator block size is invalid")
    write_started = time.perf_counter_ns()
    with path.open("wb", buffering=8 * 1024 * 1024) as output:
        for _ in range(2_048):
            output.write(block)
        output.flush()
        write_micros = (time.perf_counter_ns() - write_started) // 1_000
        fsync_started = time.perf_counter_ns()
        os.fsync(output.fileno())
        fsync_micros = (time.perf_counter_ns() - fsync_started) // 1_000
    stat = path.stat()
    hash_started = time.perf_counter_ns()
    sha256 = sha256_file(path)
    validation_hash_micros = (time.perf_counter_ns() - hash_started) // 1_000
    details = {
        "logicalBytes": stat.st_size,
        "allocatedBytes": allocated_bytes(path),
        "sha256": sha256,
        "writeMicros": write_micros,
        "fsyncMicros": fsync_micros,
        "validationHashMicros": validation_hash_micros,
    }
    if details["logicalBytes"] != 2 * 1024 * 1024 * 1024:
        raise RuntimeError(f"unexpected large-file size: {details['logicalBytes']}")
    return details


def command_large_file(args: argparse.Namespace) -> None:
    bench = Benchmark(args.root.resolve(), args.cli)
    selected = "Assets/CheckPoBenchmark/LargeBlob.bin"
    path = bench.project / selected
    if path.exists():
        raise SystemExit(f"large-file scenario path already exists: {path}")
    baseline = bench.checkpoint_list("large-file-baseline-list")[0]["checkpointId"]
    clean = bench.cli_json(
        "large-file-baseline-clean-diff",
        ["diff", str(bench.project), "--checkpoint", baseline],
    )
    assert_diff_counts(clean, added=0, modified=0, deleted=0)
    scenario_state = bench.begin_scenario("large-file-2gib", baseline)

    added = bench.local_action("large-file-add-for-discard", lambda: generate_large_file(path))
    expected = added["details"]
    diff = bench.cli_json(
        "large-file-diff-added-for-discard",
        ["diff", str(bench.project), "--checkpoint", baseline],
    )
    assert_diff_counts(diff, added=1, modified=0, deleted=0)
    discarded = apply_discard(bench, "large-file-discard-added", baseline, selected)
    if path.exists():
        raise RuntimeError("discard did not remove the large file")
    discarded_clean = bench.cli_json(
        "large-file-diff-after-discard-added",
        ["diff", str(bench.project), "--checkpoint", baseline],
    )
    assert_diff_counts(discarded_clean, added=0, modified=0, deleted=0)

    bench.local_action("large-file-add-for-checkpoint", lambda: generate_large_file(path))
    present_event = create_named_checkpoint(
        bench, "large-file-checkpoint-present", "bench-large-file-2gib"
    )
    present = (present_event.get("result") or {})["checkpointId"]
    removed = bench.local_action(
        "large-file-delete-working-tree",
        lambda: (path.unlink(), {"removedBytes": expected["logicalBytes"]})[1],
    )
    diff = bench.cli_json(
        "large-file-diff-deleted",
        ["diff", str(bench.project), "--checkpoint", present],
    )
    assert_diff_counts(diff, added=0, modified=0, deleted=1)
    restored = apply_restore(bench, "large-file-restore-deleted", present)
    restored_details = large_file_details(path)
    if restored_details["sha256"] != expected["sha256"]:
        raise RuntimeError("restored 2 GiB file hash does not match")
    clean_diff = bench.cli_json(
        "large-file-diff-after-restore",
        ["diff", str(bench.project), "--checkpoint", present],
    )
    assert_diff_counts(clean_diff, added=0, modified=0, deleted=0)

    restored_stat = path.stat()
    snapshot_mtime_ns = restored_stat.st_mtime_ns
    changed_mtime_ns = snapshot_mtime_ns + 2_000_000_000
    bench.local_action(
        "large-file-change-mtime-only",
        lambda: (
            os.utime(path, ns=(restored_stat.st_atime_ns, changed_mtime_ns)),
            {
                "beforeMtimeNs": snapshot_mtime_ns,
                "changedMtimeNs": changed_mtime_ns,
            },
        )[1],
    )
    mtime_diff = bench.cli_json(
        "large-file-diff-mtime-only",
        ["diff", str(bench.project), "--checkpoint", present],
    )
    assert_diff_counts(mtime_diff, added=0, modified=1, deleted=0)
    mtime_preview = bench.cli_json(
        "large-file-restore-mtime-only-preview",
        ["restore", "preview", str(bench.project), "--checkpoint", present],
    )
    mtime_plan = mtime_preview.get("result") or {}
    if (
        mtime_plan.get("metadataCount") != 1
        or mtime_plan.get("replaceCount") != 0
        or mtime_plan.get("stagedBytes") != 0
        or mtime_plan.get("backupBytes") != 0
    ):
        raise RuntimeError(f"mtime-only restore was not metadata-only: {mtime_plan}")
    mtime_plan_path = bench.save_plan("large-file-restore-mtime-only", mtime_plan)
    mtime_restored = bench.cli_json(
        "large-file-restore-mtime-only-apply",
        [
            "restore",
            "apply",
            str(bench.project),
            "--checkpoint",
            present,
            "--expected-plan",
            str(mtime_plan_path),
            "--yes",
        ],
    )
    metadata_only_details = large_file_details(path)
    metadata_only_details["mtimeNs"] = path.stat().st_mtime_ns
    if metadata_only_details["sha256"] != expected["sha256"]:
        raise RuntimeError("mtime-only restore changed 2 GiB file content")
    if metadata_only_details["mtimeNs"] != snapshot_mtime_ns:
        raise RuntimeError(
            "mtime-only restore did not restore snapshot mtime: "
            f"expected {snapshot_mtime_ns}, got {metadata_only_details['mtimeNs']}"
        )
    clean_mtime_diff = bench.cli_json(
        "large-file-diff-after-mtime-only-restore",
        ["diff", str(bench.project), "--checkpoint", present],
    )
    assert_diff_counts(clean_mtime_diff, added=0, modified=0, deleted=0)

    apply_restore(bench, "large-file-restore-baseline-cleanup", baseline)
    if path.exists():
        raise RuntimeError("baseline cleanup did not remove the large file")
    cleanup_diff = bench.cli_json(
        "large-file-diff-after-baseline-cleanup",
        ["diff", str(bench.project), "--checkpoint", baseline],
    )
    assert_diff_counts(cleanup_diff, added=0, modified=0, deleted=0)
    verify = bench.cli_json("large-file-verify-quick", ["verify", str(bench.project), "--quick"])
    bench.record(
        "scenario-summary",
        {
            "runId": bench.run_id,
            "scenario": "large-file-2gib",
            "baselineCheckpointId": baseline,
            "presentCheckpointId": present,
            "addMicros": added["externalMicros"],
            "deleteMicros": removed["externalMicros"],
            "expected": expected,
            "restored": restored_details,
            "discardResult": discarded.get("result"),
            "restoreResult": restored.get("result"),
            "metadataOnlyPlan": mtime_plan,
            "metadataOnlyRestoreResult": mtime_restored.get("result"),
            "metadataOnlyRestored": metadata_only_details,
            "verifyResult": verify.get("result"),
        },
    )
    bench.complete_scenario(scenario_state)
    print("large-file-2gib: complete", flush=True)


def command_restore_checkpoint(args: argparse.Namespace) -> None:
    bench = Benchmark(args.root.resolve(), args.cli)
    applied = apply_restore(bench, args.label, args.checkpoint)
    verify = bench.cli_json(
        f"{args.label}-verify-quick", ["verify", str(bench.project), "--quick"]
    )
    print(
        json.dumps(
            {
                "applied": (applied.get("result") or {}).get("applied"),
                "checkpointId": args.checkpoint,
                "verify": verify.get("result"),
            },
            ensure_ascii=False,
        ),
        flush=True,
    )


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    result.add_argument(
        "--repository",
        type=Path,
        default=Path(__file__).resolve().parents[1],
    )
    result.add_argument("--cli", type=Path, required=True)
    subcommands = result.add_subparsers(dest="command", required=True)

    prepare = subcommands.add_parser("prepare")
    prepare.add_argument("--source", type=Path, required=True)
    prepare.add_argument("--root", type=Path, required=True)
    prepare.set_defaults(function=command_prepare)

    checkpoints = subcommands.add_parser("checkpoints")
    checkpoints.add_argument("--root", type=Path, required=True)
    checkpoints.add_argument("--to", type=int, required=True)
    checkpoints.set_defaults(function=command_checkpoints)

    operations = subcommands.add_parser("operations")
    operations.add_argument("--root", type=Path, required=True)
    operations.add_argument("--milestone", type=int, required=True)
    operations.add_argument("--expected-checkpoint-count", type=int, required=True)
    operations.add_argument("--expected-latest-name", required=True)
    operations.add_argument("--full-verify", action="store_true")
    operations.set_defaults(function=command_operations)

    small_files = subcommands.add_parser("small-files")
    small_files.add_argument("--root", type=Path, required=True)
    small_files.set_defaults(function=command_small_files)

    large_file = subcommands.add_parser("large-file")
    large_file.add_argument("--root", type=Path, required=True)
    large_file.set_defaults(function=command_large_file)

    restore_checkpoint = subcommands.add_parser("restore-checkpoint")
    restore_checkpoint.add_argument("--root", type=Path, required=True)
    restore_checkpoint.add_argument("--checkpoint", required=True)
    restore_checkpoint.add_argument("--label", required=True)
    restore_checkpoint.set_defaults(function=command_restore_checkpoint)
    return result


def main() -> None:
    args = parser().parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
