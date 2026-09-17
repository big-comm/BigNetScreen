# Startup investigation

2026-09-17, native GNOME/Wayland, Intel Xe, debug build.

## Root cause and correction

The main window mapped in about 239 ms but its first frame took 3442 ms.
A GDB sample of the blocked main thread identified:

`MediaTile::init_widgets → idle callback → Pixbuf::from_file_at_scale → libglycin`.

The hidden media page scanned Pictures at startup and decoded thumbnails in
GTK idle callbacks. Idle callbacks still run on the UI thread; decoding delayed
painting even before the media page was visited.

- Load the library on the media page's first map, once per page instance.
- Decode thumbnails in background workers, capped at two concurrent decoders.
- Transfer immutable pixel bytes; construct GTK textures on the UI thread.
- Cancel queued thumbnail jobs when tiles disappear. Running decoders retain
  their semaphore permits until finished. Invalid images keep the fallback icon.
- Separate earlier fix: initialize/synchronize the discovery switch without
  emitting a user action that restarts both discovery providers.

## Measurements

Same executable profile, real GNOME session bus, actual user settings and
Pictures directory, discovery enabled. Four sequential launches before and
four after the thumbnail fix. No existing application instance was closed.

| Renderer | Before, run 1 | Before, run 2 | After, run 1 | After, run 2 |
| --- | ---: | ---: | ---: | ---: |
| Vulkan | 3446 ms | 3284 ms | 460 ms | 320 ms |
| OpenGL | 3463 ms | 3518 ms | 325 ms | 305 ms |

Values measure process startup to GTK's first `after-paint`, not compositor
presentation or device-discovery latency. No cold-cache or release-build claim.
Earlier private-bus/config measurements excluded the user's photo library and
were not representative of the reported delay. Renderer choice was not the cause;
no global graphics/theme setting was changed.

The host exports `GSK_RENDERER=vulkan`. GTK documents the override in its
[runtime configuration reference](https://docs.gtk.org/gtk4/running.html#gsk-renderer).
The dark-theme warning comes from the user's GTK settings file.

Temporary evidence: `/tmp/bignetscreen-native-*.log`,
`/tmp/bignetscreen-startup-stack.log`, `/tmp/bignetscreen-startup-probe.py`.

## Validation

Native GTK tests passed individually:

- `discovery_switch_does_not_echo_initialization_or_settings_reload`: initial
  values and settings synchronization generate no discovery event; user toggle
  generates exactly one.
- `media_library_is_lazy_and_background_thumbnails_preserve_pixels`: hidden page
  starts no scan; first map starts one scan; reopening does not restart it;
  worker limit applies; scaled RGBA pixels survive transfer into a GTK texture;
  invalid images return an error.

Run each with `cargo test -p nd-gui --locked --offline TEST_NAME -- --ignored
--test-threads=1`. They require a graphical session. Ordinary GUI tests, strict
Clippy, formatting and build checks also pass.

Updated local executable: `target/debug/bignetscreen`.
