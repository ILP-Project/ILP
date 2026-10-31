# Component fixtures

These are source fixtures for offline host ABI tests. They are intentionally not
workspace members and no WASM binaries are committed.

Build both components from the repository root (requires the target):

```sh
rustup target add wasm32-wasip2
cargo build --manifest-path fixtures/component-v1/Cargo.toml --target wasm32-wasip2 --release
cargo build --manifest-path fixtures/component-v2/Cargo.toml --target wasm32-wasip2 --release
```

Run the optional replay tests with `ILP_V1_FIXTURE` and `ILP_V2_FIXTURE` pointing
to the resulting `.wasm` files. Tests explicitly skip with a diagnostic when the
target or artifact is unavailable; normal workspace tests never access a network.
