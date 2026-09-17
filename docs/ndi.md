# NDI publishing

Requires native Linux, GStreamer's `gst-plugin-ndi`, and an NDI 5/6 runtime
installed separately under its vendor terms. BigNetScreen does not download,
link or redistribute the vendor runtime. The upstream plugin loads it dynamically.

Verify installation:

```sh
gst-inspect-1.0 ndisink
gst-inspect-1.0 ndisinkcombiner
```

Use the distribution's plugin package or follow the
[upstream build instructions](https://github.com/GStreamer/gst-plugins-rs/tree/main/net/ndi).
If the runtime is outside the library search path, set `NDI_RUNTIME_DIR_V6`
(or `NDI_RUNTIME_DIR_V5`) to its library directory before launching the app.
Linux discovery also needs the Avahi client libraries and daemon.

In Settings, choose the computer name, resolution, FPS and audio inputs.
Under Home → Publish with NDI, choose Screen, Window or Extra screen.
The receiver selects `BigNetScreen — <computer name>` from its NDI source list.
Disconnect stops publication and capture. A receiver does not need to be
present before publication starts. Sources are visible to NDI receivers on the
network; use a trusted LAN. Wired Gigabit is recommended for high bandwidth video.

This sends raw video and audio through the standard high bandwidth NDI path.
It does not implement NDI reception, HX encoding or automatic firewall changes.
The Flatpak manifest does not bundle the plugin/runtime; native use is the
supported integration target. Actual end-to-end operation requires testing with
an installed runtime and a second receiver (for example OBS + DistroAV).

Suggested hardware checks: 720p30/1080p30/custom size, microphone/system audio,
A/V synchronization, receiver join/leave, repeated Start/Stop, capture cancellation,
CPU/RSS/network use. No latency or interoperability guarantee is implied by the
synthetic pipeline tests.

Distribution must review the selected SDK's license, attribution and GPL
combination requirements before bundling any NDI components. Dynamic loading
alone does not settle compatibility. NDI® is a registered trademark of Vizrt NDI AB.

References:
- [GStreamer plugin](https://github.com/GStreamer/gst-plugins-rs/blob/main/net/ndi/README.md)
- [NDI SDK licensing](https://docs.ndi.video/all/developing-with-ndi/sdk/licensing)
- [Linux requirements](https://docs.ndi.video/all/developing-with-ndi/sdk/platform-considerations)
