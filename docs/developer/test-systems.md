# Test systems

The plans and results linked throughout these docs were measured on Suede's
own test appliances. Those machines are internal — their hostnames,
addresses, users and networks are not published — so every mention
elsewhere in these docs uses an alias (System A, System B, System C) and
links back here. This page is the map from an alias to what the hardware
actually is.

## System A {: #system-a }

- **Role:** Development workhorse
- **CPU class:** x86-64 (Intel Core i7-12700 class)
- **GPU:** NVIDIA Quadro RTX 8000 (Turing)
- **OS / kernel:** Ubuntu 26.04, kernel 7.0.0-34
- **Graphics driver:** NVIDIA proprietary, 595.91.07 (server packages)
- **Compositor:** Sway 1.11
- **Connected displays:** 4× 1920×1080 @ 59.94 Hz
- **Notable settings:** NVIDIA GSP firmware off since 2026-09-29
  (`/etc/modprobe.d/nvidia-gsp-off.conf`); boots with
  `nvidia_drm.modeset=1 fbdev=1`

## System B {: #system-b }

- **Role:** Production analog
- **CPU class:** x86-64 (amd64)
- **GPU:** NVIDIA RTX A1000 (Ampere)
- **OS / kernel:** Debian 13, kernel 6.12
- **Graphics driver:** NVIDIA 615.71.09, open kernel module (GSP on)
- **Compositor:** Sway 1.10.1
- **Connected displays:** 3× 1920×1200 @ 59.95 Hz (a fourth projector
  connector is currently disconnected)
- **Notable settings:** `allow_overlaps = true`, committed as a 2×2
  overlapping projector grid

## System C {: #system-c }

- **Role:** ARM reference
- **CPU class:** ARM (Raspberry Pi 5 Model B, 8 GB, aarch64)
- **GPU:** None (Raspberry Pi 5 integrated VideoCore, vc4/v3d)
- **OS / kernel:** Raspberry Pi OS Lite Trixie
- **Graphics driver:** Mesa (V3D), no proprietary driver
- **Compositor:** Sway 1.10.1
- **Connected displays:** two HDMI outputs (a 1080p and a 4K panel are
  available); currently run headless, with one output feeding an HDMI
  capture device for pipeline tests
- **Notable settings:** boots from NVMe rather than SD card
