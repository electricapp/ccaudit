"""Entry point for `uvx ccaudit`, `pipx run ccaudit`, `python -m ccaudit`.

Locates the platform-appropriate ``ccaudit`` binary bundled inside this
wheel and execs it, passing through argv verbatim. The binary lives at
``ccaudit/bin/ccaudit`` (or ``ccaudit.exe`` on Windows, eventually). It's
staged in by the Makefile before the wheel is built — see
``pypi/README.md`` for the build flow.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path


def _binary_path() -> Path:
    here = Path(__file__).resolve().parent
    name = "ccaudit.exe" if sys.platform == "win32" else "ccaudit"
    return here / "bin" / name


def main() -> None:
    binary = _binary_path()
    if not binary.exists():
        sys.stderr.write(
            f"ccaudit: binary not found at {binary}\n"
            "This wheel does not include a platform binary. Reinstall "
            "from a published PyPI wheel (e.g. `uvx ccaudit`), or build "
            "from source with `cargo install ccaudit`.\n"
        )
        sys.exit(1)
    # On POSIX, exec replaces our process so the user's shell ends up
    # talking to the Rust binary directly. On Windows, `os.execv` works
    # but doesn't replace the process the same way; we still hand over
    # argv verbatim.
    try:
        os.execv(str(binary), [str(binary), *sys.argv[1:]])
    except OSError as e:
        sys.stderr.write(f"ccaudit: failed to exec {binary}: {e}\n")
        sys.exit(1)


if __name__ == "__main__":
    main()
