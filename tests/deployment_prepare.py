#!/usr/bin/env python3
"""Exercise detached preparation in isolated state with an instrumented build toolchain.

No systemd jobs, activation, operator links, or real deployment state are touched.
Run after cargo build --locked -p codex-connect.
"""
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/codex-connect"


class DeploymentPrepareTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="codex-connect-prepare-")
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        self.source = self.root / "source tree"
        self.package = self.source / "crates/cli"
        self.package.mkdir(parents=True)
        (self.source / "Cargo.toml").write_text('[workspace]\nmembers=["crates/cli"]\n')
        (self.source / "Cargo.lock").write_text("locked dependencies\n")
        (self.package / "Cargo.toml").write_text('[package]\nname="codex-connect"\n')
        (self.package / "src").mkdir()
        (self.package / "src/main.rs").write_text("runtime bytes\n")
        (self.source / "config").mkdir()
        (self.source / "config/codex-cli-pin").write_text((ROOT / "config/codex-cli-pin").read_text())
        (self.source / "docs").mkdir()
        (self.source / "docs/operations.md").write_text("docs\n")
        self.dependency = self.root / "path-dependency"
        self.dependency.mkdir()
        (self.dependency / "Cargo.toml").write_text("dependency\n")
        (self.dependency / "lib.rs").write_text("dependency runtime\n")
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.sysroot = self.root / "sysroot"
        (self.sysroot / "bin").mkdir(parents=True)
        (self.sysroot / "lib").mkdir()
        (self.sysroot / "lib/libstd.rlib").write_bytes(b"standard library")
        self.build_count = self.root / "build-count"
        self.build_count.write_text("")
        self.compile_count = self.root / "compile-count"
        self.compile_count.write_text("")
        packages = [{"manifest_path": str(path / "Cargo.toml"), "source": None,
                     "targets": [{"kind": ["bin"]}]} for path in [self.package, self.dependency]]
        script = f'''#!/usr/bin/python3
import hashlib, json, os, pathlib, sys
source = pathlib.Path({str(self.source)!r})
target = pathlib.Path(os.environ["CARGO_TARGET_DIR"])
if sys.argv[1] == "metadata":
    print(json.dumps({{"packages": {packages!r}}}))
elif sys.argv[1] == "build":
    assert sys.argv[1:] == ["build", "--release", "-p", "codex-connect", "--locked"]
    assert "INVOCATION_ID" not in os.environ
    with open({str(self.build_count)!r}, "a") as counter: counter.write("build\\n")
    if (source / "fail-build").exists(): sys.exit(1)
    (target / "release/build").mkdir(parents=True, exist_ok=True)
    executable = target / "release/codex-connect"
    inputs = [source / "crates/cli/src/main.rs", pathlib.Path({str(self.dependency / 'lib.rs')!r})]
    timestamps = {{str(path): [path.stat().st_size, path.stat().st_mtime_ns] for path in inputs}}
    freshness = target / "release/.fingerprint/fixture.json"
    compiled = target / "release/deps/fixture-code"
    # Model Cargo's timestamp freshness and cached dependency artifacts. Deleting
    # just the executable can relink the same stale compiled bytes.
    fresh = freshness.exists() and compiled.exists() and json.loads(freshness.read_text()) == timestamps
    if not fresh:
        with open({str(self.compile_count)!r}, "a") as counter: counter.write("compile\\n")
        compiled.parent.mkdir(parents=True, exist_ok=True)
        compiled.write_bytes(b"".join(path.read_bytes() for path in inputs))
        freshness.parent.mkdir(parents=True, exist_ok=True)
        freshness.write_text(json.dumps(timestamps))
    if not fresh or not executable.exists():
        executable.write_bytes(b"#!/bin/sh\\nexit 0\\n# " + hashlib.sha256(compiled.read_bytes()).hexdigest().encode())
        executable.chmod(0o755)
    inputs = [source / "crates/cli/src/main.rs", source / "config/codex-cli-pin"]
    if (source / "embedded-doc").exists(): inputs.append(source / "docs/operations.md")
    depinfo = target / "release/codex-connect.d"
    depinfo.write_text(str(executable).replace(" ", "\\\\ ") + ": " + " ".join(str(p).replace(" ", "\\\\ ") for p in inputs) + "\\n")
    if (source / "mutate-during-build").exists():
        with open(source / "crates/cli/src/main.rs", "a") as runtime: runtime.write("changed during build\\n")
else: sys.exit(2)
'''
        self.write_tool("cargo", script)
        self.write_tool("rustc", f'#!/bin/sh\nif [ "$1" = -vV ]; then printf "host: x86_64-unknown-linux-gnu\\n"; else printf "%s\\n" "{self.sysroot}"; fi\n')
        for name in ["gcc", "cc", "ar", "as", "ld", "mold"]:
            self.write_tool(name, f'#!/bin/sh\ncase "$1" in -print-prog-name=*) printf "%s\\n" "{self.tools}/cc";; -print-file-name=*) printf "%s\\n" "{self.sysroot}/lib/libstd.rlib";; -E) printf "#include <...> search starts here:\\n {self.sysroot}/lib\\nEnd of search list.\\n" >&2;; esac\n')
        for name in ["cargo", "rustc"]:
            (self.sysroot / "bin" / name).write_bytes((self.tools / name).read_bytes())
        self.environment = {
            "HOME": str(self.root / "home"), "PATH": f"{self.tools}:/usr/bin:/bin",
            "XDG_STATE_HOME": str(self.root / "state"), "XDG_CACHE_HOME": str(self.root / "cache"),
            "XDG_CONFIG_HOME": str(self.root / "config"), "LANG": "C.UTF-8",
        }
        self.target = self.root / "cache/codex-connect/deploy/build"
        self.records = self.root / "state/codex-connect/deployments"
        self.records.mkdir(parents=True)
        self.operation = 0

    def write_tool(self, name, content):
        path = self.tools / name
        path.write_text(content)
        path.chmod(0o755)

    def prepare(self, success=True, **environment):
        self.operation += 1
        operation = f"{self.operation:024x}"
        record_path = self.records / f"{operation}.json"
        record_path.write_text(json.dumps({"operationId": operation, "source": str(self.source),
                                          "state": "building", "noStart": False}))
        result = subprocess.run([str(BINARY), "prepare-deployment", "--operation-id", operation,
                                 "--source", str(self.source)], env={**self.environment, **environment},
                                capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        record = json.loads(record_path.read_text())
        self.assertEqual(record["state"], "prepared" if success else "failed")
        if success:
            artifact = self.root / "home/.local/lib/codex-connect/builds" / record["sha256"][:12] / "codex-connect"
            self.assertEqual(hashlib.sha256(artifact.read_bytes()).hexdigest(), record["sha256"])
        return record

    def count(self):
        return len(self.build_count.read_text().splitlines())

    def compiles(self):
        return len(self.compile_count.read_text().splitlines())

    def test_docs_and_git_changes_reuse_verified_artifact_and_new_durable_record(self):
        first = self.prepare(INVOCATION_ID="first")
        (self.source / "docs/operations.md").write_text("different documentation\n")
        (self.source / "README.md").write_text("readme\n")
        (self.source / ".git").mkdir()
        (self.source / ".git/HEAD").write_text("different branch\n")
        second = self.prepare(INVOCATION_ID="second")
        self.assertEqual(self.count(), 1)
        self.assertEqual(first["sha256"], second["sha256"])
        self.assertNotEqual(first["operationId"], second["operationId"])
        self.assertEqual(len(list(self.records.glob("*.json"))), 2)

    def test_runtime_build_and_toolchain_inputs_invalidate(self):
        self.prepare()
        for path in [self.package / "src/main.rs", self.package / "Cargo.toml", self.source / "Cargo.lock",
                     self.source / "Cargo.toml", self.source / "config/codex-cli-pin",
                     self.dependency / "lib.rs", self.tools / "gcc", self.sysroot / "lib/libstd.rlib"]:
            with self.subTest(path=path):
                before = self.count()
                with path.open("a") as stream: stream.write("\n# change\n")
                self.prepare()
                self.assertEqual(self.count(), before + 1)
        before = self.count()
        added = self.package / "src/new.rs"
        added.write_text("new untracked runtime input\n")
        self.prepare()
        added.unlink()
        self.prepare()
        self.assertEqual(self.count(), before + 2)
        self.prepare(BUILD_MARKER="changed environment")
        self.assertEqual(self.count(), before + 3)

    def test_content_changes_with_unchanged_size_and_mtime_invalidate(self):
        first = self.prepare()
        previous = first
        for runtime, content in [(self.package / "src/main.rs", "changed bytes\n"),
                                 (self.dependency / "lib.rs", "dependency changed\n")]:
            with self.subTest(runtime=runtime):
                original = runtime.stat()
                runtime.write_text(content)
                self.assertEqual(runtime.stat().st_size, original.st_size)
                os.utime(runtime, ns=(original.st_atime_ns, original.st_mtime_ns))
                second = self.prepare()
                self.assertEqual(self.compiles(), self.count())
                self.assertNotEqual(previous["sha256"], second["sha256"])
                previous = second

    def test_config_change_and_unsupported_settings_build_normally(self):
        self.prepare()
        (self.source / ".cargo").mkdir()
        config = self.source / ".cargo/config.toml"
        config.write_text("[build]\njobs=2\n")
        self.prepare()
        self.assertEqual(self.count(), 2)
        config.write_text('[build]\nrustflags=["--cfg=custom"]\n')
        self.prepare()
        self.prepare()
        self.assertEqual(self.count(), 4)

    def test_unverified_receipts_and_outputs_force_reconstruction(self):
        first = self.prepare()
        executable = self.target / "release/codex-connect"
        receipt = self.target / "prepare-receipt.json"
        for case in ["missing", "malformed", "incomplete", "mismatched-inputs", "mismatched-digest",
                     "tampered-output", "missing-output", "unreadable-receipt", "unsupported-config"]:
            with self.subTest(case=case):
                valid = receipt.read_text()
                before = self.compiles()
                executable.write_bytes(b"tampered artifact")
                if case == "missing":
                    receipt.unlink()
                elif case == "malformed":
                    receipt.write_text("invalid JSON")
                elif case == "incomplete":
                    receipt.write_text('{"inputs":"broken"}')
                elif case in ["mismatched-inputs", "mismatched-digest"]:
                    record = json.loads(valid)
                    record["inputs" if case == "mismatched-inputs" else "sha256"] = "0" * 64
                    receipt.write_text(json.dumps(record))
                elif case == "missing-output":
                    executable.unlink()
                elif case == "unreadable-receipt":
                    receipt.unlink()
                    receipt.mkdir()
                elif case == "unsupported-config":
                    config = self.source / ".cargo/config.toml"
                    config.parent.mkdir(exist_ok=True)
                    config.write_text('[build]\nrustflags=["--cfg=custom"]\n')
                result = self.prepare(success=case != "unreadable-receipt")
                if case == "unreadable-receipt":
                    self.assertEqual(self.compiles(), before)
                    receipt.rmdir()
                    receipt.write_text(valid)
                    result = self.prepare()
                    self.assertEqual(result["sha256"], first["sha256"])
                else:
                    self.assertEqual(result["sha256"], first["sha256"])
                self.assertEqual(self.compiles(), before + 1)
                if case == "unsupported-config":
                    config.unlink()

    def test_release_cleanup_failure_prevents_preparation(self):
        self.prepare()
        release = self.target / "release"
        shutil.rmtree(release)
        release.write_bytes(b"invalid output directory")
        self.prepare(success=False)
        self.assertEqual(self.count(), 1)
        self.assertEqual(self.compiles(), 1)
        self.assertFalse((self.target / "prepare-receipt.json").exists())
        self.assertEqual(release.read_bytes(), b"invalid output directory")
        release.unlink()
        self.prepare()
        self.assertEqual(self.compiles(), 2)

    def test_failed_build_invalidates_receipt_and_records_failure(self):
        self.prepare()
        (self.package / "src/main.rs").write_text("changed\n")
        (self.source / "fail-build").touch()
        self.prepare(success=False)
        self.assertFalse((self.target / "prepare-receipt.json").exists())
        (self.source / "fail-build").unlink()
        self.prepare()
        self.assertEqual(self.count(), 3)

    def test_changes_during_build_are_not_cached(self):
        (self.source / "mutate-during-build").touch()
        self.prepare()
        self.assertFalse((self.target / "prepare-receipt.json").exists())
        self.prepare()
        self.assertEqual(self.count(), 2)

    def test_embedded_documentation_becomes_a_runtime_input(self):
        (self.source / "embedded-doc").touch()
        self.prepare()
        # The newly discovered include requires another normal build to establish a stable key.
        self.prepare()
        before = self.count()
        (self.source / "docs/operations.md").write_text("embedded runtime content\n")
        self.prepare()
        self.assertEqual(self.count(), before + 1)
        self.prepare()
        self.assertEqual(self.count(), before + 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
