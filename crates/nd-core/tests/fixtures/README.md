# Encoded test fixtures

`nvenc-idr-160x96.h264` is one Annex B access unit (AUD, SPS, PPS, IDR) from
`nvh264enc` at 160x96, constrained baseline, as BigNetScreen configures it.
Its SPS has a VUI without `bitstream_restriction` and three reference frames,
the case `pipeline::decode_buffer` rewrites. Regenerate it on an NVIDIA machine
with:

```sh
gst-launch-1.0 videotestsrc num-buffers=2 \
  ! video/x-raw,width=160,height=96,framerate=60/1 \
  ! nvh264enc preset=low-latency-hq rc-mode=cbr bitrate=500 gop-size=60 \
      bframes=0 zerolatency=true aud=true \
  ! video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au \
  ! filesink location=out.h264
```

and keep the bytes before the second access unit delimiter.
