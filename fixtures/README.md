# Component fixtures

These are source fixtures for offline host ABI tests. They are intentionally not
workspace members and no WASM binaries are committed.

Build both components from the repository root. The fixtures are deliberately
standalone Cargo packages (not workspace members), so the commands must use each
fixture's manifest path:

```sh
rustup target add wasm32-wasip2
cargo build --manifest-path fixtures/component-v1/Cargo.toml --target wasm32-wasip2 --release
cargo build --manifest-path fixtures/component-v2/Cargo.toml --target wasm32-wasip2 --release
```

The generated files are intentionally untracked:

```text
fixtures/component-v1/target/wasm32-wasip2/release/ilp_fixture_component_v1.wasm
fixtures/component-v2/target/wasm32-wasip2/release/ilp_fixture_component_v2.wasm
```

If Cargo reports that a fixture "believes it's in a workspace", use the commands
above with the checked-in fixture manifests; each manifest contains an empty
`[workspace]` table to keep the package out of the root workspace while making
root-invoked `--manifest-path` builds valid. The fixture sources generate their
component bindings from the checked-in WIT definitions and do not require a
separate `wit-bindgen-cli` installation.

Run the optional host component tests with `ILP_V1_FIXTURE` and `ILP_V2_FIXTURE`
pointing to the resulting `.wasm` files, or let the tests use the paths above.
When an artifact is missing, tests explicitly print the expected path and the
corresponding build command before skipping; normal workspace tests never access
a network.
