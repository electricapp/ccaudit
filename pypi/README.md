# PyPI packaging

One Python package (`ccaudit`), many platform-specific wheels. Mirrors
the ruff / rye pattern: the wheel tag picks the right binary at install
time, so `uvx ccaudit` and `pipx run ccaudit` just work.

## Layout

```
pypi/ccaudit/
├── pyproject.toml           # metadata + entry point
├── setup.py                 # thin shim so --plat-name works
├── MANIFEST.in              # keeps bin/ in the sdist
└── src/ccaudit/
    ├── __init__.py
    ├── __main__.py          # finds bin/ccaudit, execs it
    └── bin/                 # populated at build time (one binary per wheel)
```

## Build flow

Binaries come from `make cross` (zigbuild). For each target:

1. `make pypi-stage-<target>` — copy the matching binary into `pypi/ccaudit/src/ccaudit/bin/ccaudit`
2. `make pypi-wheel-<target>` — run `python setup.py bdist_wheel --plat-name=<tag>` to produce a tagged wheel
3. Collect wheels in `pypi/ccaudit/dist/`

Or: `make pypi-wheels` to do all four targets in sequence.

Platform → PEP 425 wheel tag:

| Rust target                        | Wheel `--plat-name`        |
| ---------------------------------- | -------------------------- |
| `aarch64-apple-darwin`             | `macosx_11_0_arm64`        |
| `x86_64-apple-darwin`              | `macosx_10_12_x86_64`      |
| `x86_64-unknown-linux-musl`        | `manylinux_2_17_x86_64`    |
| `aarch64-unknown-linux-musl`       | `manylinux_2_17_aarch64`   |

musl-linked Linux binaries are portable across glibc versions, so the
`manylinux_2_17_*` tag is conservative but accurate — PyPI accepts
these without `auditwheel repair`.

## Publishing

Once the wheels exist and are smoke-tested:

```bash
uv publish pypi/ccaudit/dist/*.whl        # or twine upload
```

For the CI path, see `.github/workflows/release.yml` (TODO: add a
`pypi-publish` job mirroring the existing `npm-publish` one — the build
artifacts from the matrix are already tagged by target).

## Local smoke test

```bash
make pypi-stage-$(uname -s)-$(uname -m)    # stage host binary
make pypi-wheel-$(uname -s)-$(uname -m)    # build wheel
uvx --from pypi/ccaudit/dist/ccaudit-*.whl ccaudit --help
```
