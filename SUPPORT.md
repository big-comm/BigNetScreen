# Help and compatibility reports

## A receiver is not listed

Check its Cast/Screen Mirroring mode, the selected protocol and whether automatic discovery is enabled. Cast needs local multicast discovery and a route back from the receiver; guest Wi-Fi isolation or VPN routing can prevent both. Miracast uses Wi-Fi Direct and needs compatible NetworkManager/hardware on a native installation. Do not disable your firewall globally as a troubleshooting step.

## Video is slow, freezes, or is black

Record which path is active: Cast mirroring, Cast HTTP, WFD, browser or NDI. For Cast HTTP, seconds of receiver buffering are possible. Try a modest resolution and compare a still desktop with motion. Record the negotiated resolution/FPS and encoder; available CPU/GPU capacity alone does not prove network/decoder capacity. Test the same receiver after Stop and a new connection.

A missing GStreamer factory is a dependency problem. WebRTC needs `nicesink` and `nicesrc` from the GStreamer libnice plugin, not just the shared libnice library. NDI needs its separate runtime; [NDI setup](docs/ndi.md) describes that optional path. Portal/virtual-screen failures need the desktop, compositor and portal backend versions.

## Safe diagnostic commands

```sh
bignetscreen --version
rustc --version   # only for source-build reports
# GStreamer tools, when installed:
gst-inspect-1.0 --version
gst-inspect-1.0 pipewiresrc
gst-inspect-1.0 nicesink
```

For a short, private reproduction, enable `BIGNETSCREEN_FPS_LOG=1` and/or `BIGNETSCREEN_LATENCY=1`. These report sender-side diagnostics, not display latency. Review logs before attaching them; addresses, filenames, device names and selected content can be sensitive. Avoid trace-level packet dumps in public reports.

Include the exact commit/package version, distribution, desktop/portal, encoder/driver, receiver model and firmware, source/negotiated mode, transport, network arrangement and steps to reproduce. State whether Stop, window close, reconnection, network loss and a sustained session were tested. A result against one receiver is not a result against all firmware generations.
