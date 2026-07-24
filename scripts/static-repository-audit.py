#!/usr/bin/env python3
"""Dependency-free structural audit for the Sisyphus source tree."""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE_ROOTS = ("core", "kernel", "libraries", "userland", "tools")
DEBRIS_SUFFIXES = (".orig", ".rej", ".rlib", ".rmeta", ".swp", "~")


def fail(message: str, failures: list[str]) -> None:
    failures.append(message)


def tracked_files() -> list[str]:
    completed = subprocess.run(
        ["git", "ls-files"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return [line for line in completed.stdout.splitlines() if line]


def rust_files() -> list[Path]:
    files: list[Path] = []
    for directory in SOURCE_ROOTS:
        base = ROOT / directory
        if not base.exists():
            continue
        files.extend(
            path
            for path in base.rglob("*.rs")
            if "target" not in path.parts and ".git" not in path.parts
        )
    return sorted(files)


def matching_delimiters(text: str) -> tuple[bool, str]:
    pairs = {")": "(", "]": "[", "}": "{"}
    stack: list[tuple[str, int]] = []
    index = 0
    line = 1

    while index < len(text):
        character = text[index]
        if character == "\n":
            line += 1

        if text.startswith("//", index):
            newline = text.find("\n", index)
            index = len(text) if newline < 0 else newline
            continue

        if text.startswith("/*", index):
            depth = 1
            index += 2
            while index < len(text) and depth:
                if text.startswith("/*", index):
                    depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    if text[index] == "\n":
                        line += 1
                    index += 1
            if depth:
                return False, f"unterminated block comment near line {line}"
            continue

        if character == '"':
            index += 1
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                    continue
                if text[index] == '"':
                    index += 1
                    break
                if text[index] == "\n":
                    line += 1
                index += 1
            continue

        if character == "'":
            probe = index + 1
            if probe < len(text) and text[probe] == "\\":
                probe += 2
            else:
                probe += 1
            if probe < len(text) and text[probe] == "'":
                index = probe + 1
                continue

        if character in "([{":
            stack.append((character, line))
        elif character in ")]}" :
            if not stack or stack[-1][0] != pairs[character]:
                return False, f"mismatched {character!r} at line {line}"
            stack.pop()

        index += 1

    if stack:
        opening, opening_line = stack[-1]
        return False, f"unclosed {opening!r} from line {opening_line}"
    return True, ""


def code_only(text: str) -> str:
    output = list(text)
    index = 0
    while index < len(text):
        if text.startswith("//", index):
            end = text.find("\n", index)
            end = len(text) if end < 0 else end
            for position in range(index, end):
                output[position] = " "
            index = end
            continue
        if text.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(text) and depth:
                if text.startswith("/*", end):
                    depth += 1
                    end += 2
                elif text.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            for position in range(index, min(end, len(text))):
                if output[position] != "\n":
                    output[position] = " "
            index = end
            continue
        if text[index] == '"':
            end = index + 1
            while end < len(text):
                if text[end] == "\\":
                    end += 2
                    continue
                if text[end] == '"':
                    end += 1
                    break
                end += 1
            for position in range(index, min(end, len(text))):
                if output[position] != "\n":
                    output[position] = " "
            index = end
            continue
        index += 1
    return "".join(output)


def production_prefix(text: str) -> str:
    return text.split("#[cfg(test)]", 1)[0]


def audit_rust(path: Path, failures: list[str]) -> None:
    text = path.read_text(encoding="utf-8")
    relative = path.relative_to(ROOT)

    balanced, detail = matching_delimiters(text)
    if not balanced:
        fail(f"{relative}: {detail}", failures)

    production = production_prefix(text)
    policy_source = relative.as_posix() == "tools/reality-gate/src/main.rs"
    unfinished = (
        "todo!(",
        "unimplemented!(",
        "STUBS" + " FOR",
        "Pret" + "end ",
        "mock" + " of",
    )
    for marker in unfinished:
        if not policy_source and marker in production:
            fail(f"{relative}: production marker {marker!r}", failures)

    if not policy_source and re.search(
        r"#!\s*\[allow\([^]]*\bdead_code\b[^]]*\)\]",
        production,
        re.DOTALL,
    ):
        fail(f"{relative}: module-wide dead-code suppression", failures)

    source_code = code_only(text)
    for match in re.finditer(r"\bfn\s+([^\s(<]+)", source_code):
        name = match.group(1)
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
            line = text[: match.start()].count("\n") + 1
            fail(f"{relative}:{line}: invalid function name {name!r}", failures)

    if path.name not in {"lib.rs", "main.rs", "mod.rs"}:
        return

    for match in re.finditer(
        r"(?m)^\s*(?:pub\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;",
        text,
    ):
        module = match.group(1)
        flat = path.parent / f"{module}.rs"
        nested = path.parent / module / "mod.rs"
        if not flat.is_file() and not nested.is_file():
            line = text[: match.start()].count("\n") + 1
            fail(
                f"{relative}:{line}: module {module!r} has no source file",
                failures,
            )


def main() -> int:
    failures: list[str] = []

    for tracked in tracked_files():
        if tracked.endswith(DEBRIS_SUFFIXES):
            fail(f"tracked source debris: {tracked}", failures)

    files = rust_files()
    for path in files:
        audit_rust(path, failures)

    if failures:
        print("Sisyphus structural audit FAILED", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1

    print("Sisyphus structural audit PASS")
    print(f"rust_files={len(files)}")
    print(f"tracked_files={len(tracked_files())}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
