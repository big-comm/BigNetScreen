#!/usr/bin/env python3
"""Read supplied encoder artifacts; no quality, speed or hardware inference."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

p=argparse.ArgumentParser();p.add_argument('evidence',type=Path);args=p.parse_args()
result={'warning':'These files were supplied, not encoded by the patched Rust program.', 'streams':{}, 'qp_sizes':{}}
for name in ['atual.h264','novo.h264']:
 path=args.evidence/'h264'/name
 data=path.read_bytes()
 nals=re.split(b'\x00\x00\x00\x01|\x00\x00\x01',data)
 units=[];size=0;is_key=False
 # Reuse the exact AUD-to-AUD byte lengths, including all NAL start codes.
 starts=[m.start() for m in re.finditer(b'\x00\x00(?:\x00)?\x01\x09',data)]
 for start,end in zip(starts, starts[1:]+[len(data)]):
  unit=data[start:end]
  key=any(n and n[0]&31==5 for n in re.split(b'\x00\x00\x00\x01|\x00\x00\x01',unit))
  units.append((len(unit),key))
 keys=[size for size,key in units if key]
 ffprobe=subprocess.run(['ffprobe','-v','error','-count_frames','-show_entries',
   'stream=codec_name,profile,width,height,r_frame_rate,nb_read_frames','-of','json',str(path)],
   check=True,capture_output=True,text=True,timeout=20)
 result['streams'][name]={'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),
   'access_units':len(units),'keyframes':len(keys),'mean_I_bytes':sum(keys)/len(keys),'max_I_bytes':max(keys),
   'packets_at_current_1181_byte_chunk':(max(keys)+1180)//1181,'ffprobe':json.loads(ffprobe.stdout)}
for scene in ['ui','src']:
 base=args.evidence/'h264'/f'q_{scene}_base.h264'
 record={}
 for mode in ['base','cabac','p5']:
  path=args.evidence/'h264'/f'q_{scene}_{mode}.h264'
  record[mode]=({'bytes':path.stat().st_size,'change_percent':100*(path.stat().st_size/base.stat().st_size-1),
                 'sha256':hashlib.sha256(path.read_bytes()).hexdigest()} if path.exists() else 'NOT_SUPPLIED')
 result['qp_sizes'][scene]=record
print(json.dumps(result,indent=2))
