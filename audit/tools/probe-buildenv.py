#!/usr/bin/env python3
"""Check pkg-config, real native linking and runtime loading without Rust or GUI."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile

# Stable exported functions; no configured headers or display server needed.
PROBES = {
    "core": (
        ["gstreamer-1.0", "gstreamer-app-1.0", "gstreamer-video-1.0", "gstreamer-audio-1.0"],
        "extern void gst_init(void*,void*);\n"
        "extern unsigned long gst_app_sink_get_type(void);\n"
        "extern unsigned long gst_video_info_get_type(void);\n"
        "extern unsigned long gst_audio_info_get_type(void);\n"
        "int main(void){gst_init(0,0);return !(gst_app_sink_get_type() && "
        "gst_video_info_get_type() && gst_audio_info_get_type());}\n",
    ),
    "gui": (
        ["gtk4", "libadwaita-1", "graphene-1.0"],
        "extern unsigned gtk_get_major_version(void);\n"
        "extern unsigned adw_get_major_version(void);\n"
        "extern void *graphene_rect_alloc(void);\n"
        "extern void graphene_rect_free(void*);\n"
        "int main(void){void *r=graphene_rect_alloc();graphene_rect_free(r);"
        "return !(gtk_get_major_version()==4 && adw_get_major_version()>=1);}\n",
    ),
}


def command(args: list[str], **kwargs) -> dict:
    try:
        proc = subprocess.run(args, capture_output=True, text=True, timeout=20,
                              check=False, **kwargs)
        return {"command": args, "exit_code": proc.returncode,
                "stdout": proc.stdout.strip(), "stderr": proc.stderr.strip()}
    except (OSError, subprocess.TimeoutExpired) as exc:
        return {"command": args, "error": str(exc), "exit_code": 78}


def probe(scope: str, root: Path) -> dict:
    report = {"status": "PASS_NATIVE_PROBES_ONLY", "scope": scope, "probes": {},
              "warning": "This checks selected native prerequisites, not Rust compilation or the GUI."}
    scopes = ("core", "gui") if scope == "workspace" else ("core",)
    with tempfile.TemporaryDirectory(prefix="native-probe-", dir=root) as temp:
        work = Path(temp)
        for name in scopes:
            packages, source = PROBES[name]
            info = {"pkg_config": command(["pkg-config", "--modversion", *packages])}
            flags = command(["pkg-config", "--libs", *packages])
            info["link_flags"] = flags
            if info["pkg_config"]["exit_code"] or flags["exit_code"]:
                info["status"] = "DEPENDENCY_PKG_CONFIG"
            else:
                src, exe = work / f"{name}.c", work / name
                src.write_text(source)
                # CC may be a documented compiler command plus flags; never use shell=True.
                compiler = shlex.split(os.environ.get("CC", "cc"))
                info["link"] = command([*compiler, str(src), "-o", str(exe),
                                        *shlex.split(flags["stdout"])])
                if info["link"]["exit_code"]:
                    info["status"] = "NATIVE_LINK_FAILED_SEE_LOG"
                else:
                    info["run"] = command([str(exe)])
                    info["status"] = "PASS" if not info["run"]["exit_code"] else "NATIVE_RUN_FAILED_SEE_LOG"
            report["probes"][name] = info
            if info["status"] != "PASS":
                report["status"] = "PREREQUISITE_CHECK_FAILED"
        # Informational only: generated Rust FFI may not require this C header.
        cflags = command(["pkg-config", "--cflags", "glib-2.0"])
        dirs = [f[2:] for f in shlex.split(cflags.get("stdout", "")) if f.startswith("-I")]
        report["glibconfig_header"] = {
            "present": any((Path(d) / "glibconfig.h").is_file() for d in dirs),
            "include_dirs": dirs,
            "required_for": "C code including glib.h; not automatically a Rust build blocker",
        }
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scope", choices=("core", "workspace"), default="core")
    parser.add_argument("--work-dir", type=Path, required=True,
                        help="Existing executable directory (avoid a noexec /tmp)")
    args = parser.parse_args()
    report = probe(args.scope, args.work_dir)
    print(json.dumps(report, indent=2))
    return 0 if report["status"] == "PASS_NATIVE_PROBES_ONLY" else 78


if __name__ == "__main__":
    raise SystemExit(main())
