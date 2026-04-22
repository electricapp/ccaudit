# Minimal setuptools shim. Everything metadata-wise lives in
# pyproject.toml; this file exists only so we can run
# `python setup.py bdist_wheel --plat-name=<tag>` at build time to tag
# wheels per platform. PEP 517 tools don't expose --plat-name cleanly,
# so we keep this escape hatch.
from setuptools import setup

setup()
