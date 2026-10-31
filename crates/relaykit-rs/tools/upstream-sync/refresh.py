"""Refresh offline Go oracles using the module pinned in this directory."""

import argparse
from datetime import date
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
MODULE = "github.com/QuantumNous/new-api/relaykit"
UUID = re.compile(r"[0-9a-fA-F]{8}(?:-?[0-9a-fA-F]{4}){3}-?[0-9a-fA-F]{12}")


def normalized(result, case):
    source = json.dumps(case.get("input"), separators=(",", ":"))
    aliases = {}

    def visit(value, key=""):
        if isinstance(value, dict):
            if "Type" in value and "Payload" in value:
                return visit(value["Payload"])
            return {name: visit(value[name], name) for name in sorted(value)}
        if isinstance(value, list):
            return [visit(item) for item in value]
        if key in ("id", "call_id") and isinstance(value, str):
            def replace(match):
                token = match.group()
                if token in source:
                    return token
                identity = token.replace("-", "").lower()
                return aliases.setdefault(identity, f"<uuid_{len(aliases)}>")
            return UUID.sub(replace, value)
        if key in ("created", "created_at") and isinstance(value, (int, float)):
            if value >= 100_000_000 and str(value) not in source:
                return "<timestamp>"
        return value

    output = visit({
        "value": result.get("value"),
        "usage": result.get("usage"),
        "diagnostics": result.get("diagnostics") or [],
        "converter": result.get("converter"),
        "quality": result.get("quality"),
        "error": "error" in result,
    })
    if case["name"].startswith("native_group/"):
        for diagnostic in output["diagnostics"]:
            if "path" in diagnostic:
                diagnostic["path"] = re.sub(r"^tools\[\d+\]", "tools[*]", diagnostic["path"])
    output["diagnostics"].sort(key=lambda item: json.dumps(item, sort_keys=True))
    return output


def encode_case(case):
    payload = {key: value for key, value in case.items() if key not in ("raw_pointer", "input_json")}
    if payload["kind"] == "stream" and "chunks" not in payload:
        payload["chunks"] = payload["input"]
    if "input_json" not in case:
        return json.dumps(payload, separators=(",", ":"))
    payload.pop("input", None)
    # Splice raw input instead of decoding it through Python's float parser.
    return json.dumps(payload, separators=(",", ":"))[:-1] + ',"input":' + case["input_json"] + "}"


def run_oracle(oracle, entries):
    process = subprocess.run(
        [str(oracle)],
        input="\n".join(encode_case(entry["case"]) for entry in entries) + "\n",
        text=True, capture_output=True, check=True,
    )
    results = [json.loads(line) for line in process.stdout.splitlines()]
    if len(results) != len(entries):
        raise RuntimeError("Go oracle returned the wrong number of results")
    for entry, result in zip(entries, results):
        if result["name"] != entry["case"]["name"]:
            raise RuntimeError("Go oracle returned cases out of order")
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--verified-head")
    parser.add_argument("--path-commit")
    parser.add_argument("--all", action="store_true", help="also refresh existing parity corpora")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit):
        parser.error("--commit must be a full Git commit SHA")
    metadata = json.loads(subprocess.check_output(
        ["go", "mod", "download", "-json", MODULE], cwd=HERE, text=True,
    ))
    if not metadata["Version"].endswith(args.commit[:12]):
        raise RuntimeError("Go module pin does not match --commit")
    module_dir = Path(metadata["Dir"])
    source_hash = hashlib.sha256()
    for source in sorted(module_dir.rglob("*.go")):
        source_hash.update(str(source.relative_to(module_dir)).encode() + b"\0")
        source_hash.update(hashlib.sha256(source.read_bytes()).digest())
    provenance = {
        "repository": "https://github.com/QuantumNous/new-api",
        "path": "relaykit",
        "baseline_tag": "relaykit/v0.3.5",
        "baseline_commit": "d8e65613c09adf3663f960ea4316414a99694751",
        "target_module": metadata["Version"],
        "target_commit": args.commit,
        "latest_relaykit_path_commit": args.path_commit or args.commit,
        "verified_repository_head": args.verified_head or args.commit,
        "go_version": subprocess.check_output(["go", "version"], text=True).strip().removeprefix("go version "),
        "source_go_sha256": source_hash.hexdigest(),
        "oracle_sha256": hashlib.sha256((HERE / "main.go").read_bytes()).hexdigest(),
        "generated_on": date.today().isoformat(),
    }
    with tempfile.TemporaryDirectory(prefix="relaykit-oracle-") as temporary:
        oracle = Path(temporary) / "oracle"
        subprocess.run(["go", "build", "-o", str(oracle), "."], cwd=HERE, check=True)
        total = changed = 0
        for fixture in sorted((ROOT / "tests/fixtures").glob("*.json")):
            corpus = json.loads(fixture.read_text())
            sync = fixture.name == "upstream_sync.json"
            if not sync and not args.all:
                continue
            entries = corpus["cases"] if sync else corpus
            if not isinstance(entries, list) or not entries or not all(
                isinstance(entry, dict) and "case" in entry and "expected" in entry for entry in entries
            ):
                continue
            updates = 0
            for entry, result in zip(entries, run_oracle(oracle, entries)):
                if sync or normalized(entry["expected"], entry["case"]) != normalized(result, entry["case"]):
                    entry["expected"] = result
                    updates += 1
            if sync:
                corpus["provenance"] = provenance
            if updates:
                fixture.write_text(json.dumps(corpus, indent=2, ensure_ascii=True) + "\n")
            print(f"{fixture.name}: checked {len(entries)}, updated {updates}")
            total += len(entries)
            changed += updates
        print(f"Checked {total} pinned-Go cases; updated {changed} expectations")


if __name__ == "__main__":
    main()
