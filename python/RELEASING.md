# Releasing the Python package

How `pip install bridge-solver` gets its files, and the one-time setup it
needs. Every release after that is automatic, triggered by the version tag
like everything else.

## How a release reaches PyPI

When a tag like `v2.1.0` is pushed, `.github/workflows/release.yml` runs these
jobs:

1. **`check-version`** fails the release unless `Cargo.toml`,
   `wasm/Cargo.toml`, `ffi/Cargo.toml` and `python/Cargo.toml` all say the
   tag's version. PyPI never accepts the same version twice, so a mistake
   caught after upload could not be fixed, only skipped past.
2. **`build-python`** builds one wheel per platform:
   - Linux x86-64 and ARM (manylinux),
   - macOS Intel and Apple Silicon,
   - Windows x86-64.

   Each is an `abi3` wheel, so one wheel serves every Python from 3.9 on.
3. **`build-python-sdist`** builds the source package. pip falls back to it on
   a platform with no wheel, which needs a Rust toolchain there.
4. **`publish-python`** uploads everything to PyPI using *trusted publishing*.
   PyPI is told once that this repository's `release.yml` may publish
   `bridge-solver`, so no password or token is stored anywhere.

## One-time setup (before the first release)

1. **A PyPI account.** Create one at https://pypi.org/account/register/ and
   turn on two-factor authentication; PyPI requires it.
2. **A pending publisher.** The name `bridge-solver` is not taken yet, so add
   a pending publisher, which reserves it for this workflow:
   - go to https://pypi.org/manage/account/publishing/;
   - fill in the form:
     - PyPI project name: `bridge-solver`
     - Owner: `bridge-craftwork`
     - Repository name: `bridge-solver`
     - Workflow name: `release.yml`
     - Environment name: `pypi`
   - The first successful publish creates the project, with you as owner.
3. **A GitHub environment.** In the repository, open **Settings →
   Environments → New environment** and name it `pypi`. Optionally, add
   yourself under *Required reviewers*. Each release then pauses for your
   click before anything goes to PyPI, which is a good safety net for the
   first few.

To rehearse first, do the same on https://test.pypi.org. Then temporarily
add `with: repository-url: https://test.pypi.org/legacy/` to the publish
step. Installing from TestPyPI is
`pip install -i https://test.pypi.org/simple/ bridge-solver`.

## Each release

1. Bump `version` in all four `Cargo.toml` files. Rebuild each lock file:
   - for the root crate, with `./dev-build.sh --ci build`;
   - for the others, with
     `./dev-build.sh --ci build --manifest-path <dir>/Cargo.toml`.

   Then commit.
2. Tag and push: `git tag v2.1.0 && git push origin v2.1.0`.
3. Watch the **Release** workflow; approve the `pypi` step if you required a
   reviewer.
4. Check it: https://pypi.org/project/bridge-solver/, then in a fresh virtual
   environment, `pip install bridge-solver==2.1.0`.

## Working on the package locally

```sh
python3 -m venv .venv && . .venv/bin/activate
pip install maturin pytest
# Move the local [patch] overrides aside, so python/Cargo.lock keeps its
# published sources (see CLAUDE.md), then build into the venv and test.
mv .cargo/config.toml .cargo/config.toml.off
(cd python && maturin develop --release)
mv .cargo/config.toml.off .cargo/config.toml
pytest python/tests
```

`maturin build --release --manifest-path python/Cargo.toml --out dist` makes
the wheel itself, to inspect or to install elsewhere with
`pip install dist/*.whl`.
