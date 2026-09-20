#!/usr/bin/env python3
"""Exercise installed GStreamer through its public C ABI; no headers/toolchain needed."""
import ctypes as C, ctypes.util, json

def bind(lib, name, ret, args):
    f=getattr(lib,name); f.restype=ret; f.argtypes=args; return f
P=C.c_void_p; U=C.c_uint64
G=C.CDLL(ctypes.util.find_library('gstreamer-1.0'))
A=C.CDLL(ctypes.util.find_library('gstapp-1.0'))
init=bind(G,'gst_init',None,[P,P]); init(None,None)
launch=bind(G,'gst_parse_launch',P,[C.c_char_p,C.POINTER(P)])
by_name=bind(G,'gst_bin_get_by_name',P,[P,C.c_char_p])
state=bind(G,'gst_element_set_state',C.c_int,[P,C.c_int])
pull=bind(A,'gst_app_sink_try_pull_sample',P,[P,U])
buffer=bind(G,'gst_sample_get_buffer',P,[P]); segment=bind(G,'gst_sample_get_segment',P,[P])
running=bind(G,'gst_segment_to_running_time',U,[P,C.c_int,U])
unref_sample=bind(G,'gst_mini_object_unref',None,[P]); unref=bind(G,'gst_object_unref',None,[P])
class Mini(C.Structure):
    _fields_=[('type',C.c_size_t),('refs',C.c_int),('lock',C.c_int),('flags',C.c_uint),('copy',P),('dispose',P),('free',P),('private',C.c_uint),('ptr',P)]
class Buffer(C.Structure):
    _fields_=[('mini',Mini),('pool',P),('pts',U),('dts',U),('duration',U),('offset',U),('offset_end',U)]
assert C.sizeof(Mini)==64 and Buffer.pts.offset==72
results=[]
for encoder,profile in [('x264enc tune=zerolatency speed-preset=ultrafast threads=2 bframes=0 cabac=true','high'),('openh264enc','high'),('openh264enc','constrained-baseline')]:
    error=P()
    desc=f'videotestsrc num-buffers=6 ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 ! {encoder} ! video/x-h264,profile={profile},stream-format=byte-stream,alignment=au ! appsink name=probe sync=false'
    pipeline=launch(desc.encode(),C.byref(error))
    entry={'pipeline':desc,'parse_error':bool(error),'frames':[]}; results.append(entry)
    if not pipeline: continue
    sink=by_name(pipeline,b'probe')
    try:
        entry['state_result']=state(pipeline,4)
        for i in range(6):
            sample=pull(sink,1_000_000_000)
            if not sample: break
            try:
                b=C.cast(buffer(sample),C.POINTER(Buffer)).contents
                entry['frames'].append({'pts_ns':b.pts,'running_ns':running(segment(sample),3,b.pts)})
            finally: unref_sample(sample)
    finally:
        state(pipeline,1); unref(sink); unref(pipeline)
print(json.dumps(results,indent=2))
assert len(results[0]['frames'])==6, 'x264 native pipeline did not deliver six frames'
assert results[0]['frames'][0]['running_ns']==0
assert results[0]['frames'][0]['pts_ns']!=0, 'Expected encoder timestamp offset not observed'
