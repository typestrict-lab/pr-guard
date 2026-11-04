# Development

## Examples

`examples/` has small projects in several languages with pull requests that plant real problems (SQL injection, a
removed permission check, a renamed public function, a description that says something else) next to clean ones.

```sh
bash examples/run.sh
```

## Development

```sh
cargo test                                         # parity tests skip without the weights
PR_GUARD_MODEL=/path/to/lab-model cargo test --release   # Rust/Python parity on the golden vectors
```

## Releases

A `v*` tag (matching `version` in `Cargo.toml`) runs `.github/workflows/release.yml`: it builds every target, runs each
archive against the real model, attaches the archives, their `.sha256`, `SHA256SUMS` and the Homebrew formula to the
GitHub release, and pushes the multi-arch image to `ghcr.io/typestrict-lab/pr-guard` (plus the Bitbucket Pipe image).
`scripts/fetch-model.sh` pins the model revision and hashes for the image and the smoke test; bump it with `src/pull.rs`.

## How ONNX Runtime is linked

`ort` with `download-binaries` links pyke's prebuilt ONNX Runtime (1.28) **statically** where pyke ships one; elsewhere
the build links Microsoft's shared library and the archive carries it next to the binary.

| Target | ONNX Runtime | Ships next to the binary | Other runtime dependencies |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | static (pyke) | nothing | glibc, libstdc++, libgcc_s |
| `aarch64-unknown-linux-gnu` | static (pyke) | nothing | glibc, libstdc++, libgcc_s |
| `aarch64-apple-darwin` | static (pyke, CoreML build) | nothing | system libc++, Foundation, CoreML |
| `x86_64-apple-darwin` | shared: Microsoft's 1.23.2, their last macOS x64 build (`ORT_LIB_PATH`, `ORT_PREFER_DYNAMIC_LINK`, rpath `@executable_path`); cross-compiled on Apple silicon | `libonnxruntime.1.23.2.dylib` | system libc++ |
| `x86_64-pc-windows-msvc` | static (pyke, DirectML build) | `DirectML.dll` (imported at load; Windows' own copy is older) | the Visual C++ runtime (`vcruntime140.dll`), as any Rust MSVC binary |

pr-guard asks for ONNX Runtime's C API version 17, so the 1.23.2 library on macOS x64 is enough and gives the same
answers as the static 1.28 build (same probabilities on a smoke diff, run under Rosetta). There is no musl build: pyke ships none and
Microsoft's Linux builds link glibc.
