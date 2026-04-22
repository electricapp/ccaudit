"""ccaudit — Fast Claude Code log viewer.

This package is a thin Python shell around a pre-built Rust binary.
See ``ccaudit.__main__`` for the entry point.

Version is declared once in `Cargo.toml` and propagated to
`pypi/ccaudit/pyproject.toml` and `npm/*/package.json` by
`make sync-versions`. Read it at runtime with:

    from importlib.metadata import version
    version("ccaudit")
"""
