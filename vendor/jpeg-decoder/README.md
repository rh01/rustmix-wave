Vendored `jpeg-decoder` 0.3.2 (MIT OR Apache-2.0).

The crates.io crate decodes on non-wasm targets with `MpscWorker`, which
spawns one `std::thread` per component. Those threads inherit the restored
pthread config (4 KiB internal stack) and overflow in the IDCT. This copy
always selects `PreferWorkerKind::Immediate`, and `spawn_worker_thread`
returns an error if that path is ever reached. Decode stays on `weread-img`.
