# Local Relm4 patch

This is Relm4 0.11.0 from crates.io, with its original MIT and Apache licenses.
The workspace patches that same version to this directory; it is tracked source.

`RuntimeSenders::new` removes closed shutdown channels before registering the
next component. Upstream keeps every sender until application shutdown, retaining
512 bytes per completed component in the measured build. Recreating five
BigNetScreen components across 20 additional windows retained 51,200 bytes in
the short/long heaptrack comparison, even after the widgets finalized.

The registry now retains channels for live components and, until the next
registration, the most recently closed batch. Its allocation follows peak
concurrent component count rather than lifetime creation count. Shutdown delivery
to live components is unchanged.

Run the focused regression with:

```sh
cargo test --manifest-path vendor/relm4/Cargo.toml --locked --offline --lib completed_components_do_not_accumulate_shutdown_channels
```

This standalone upstream test needs the crate's optional and development
dependencies cached, including `libpanel`; the workspace build does not.

Remove the override when the locked upstream release bounds this registry.
See `docs/testing.md` in the workspace for the real window finalization gate.
