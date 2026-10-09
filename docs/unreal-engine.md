# Unreal Engine applications

Use a packaged Linux Vulkan application as a normal Wayland window. Suede's existing Command launcher supervises it and projects its window across the configured canvas. No custom texture-sharing plugin or renderer backend is required.

## Author an unattended application

Set frame pacing in the project's configuration. For example, these entries
in `Config/DefaultEngine.ini` request a 60 FPS cap without VSync:

```ini
[SystemSettings]
r.VSync=0
t.MaxFPS=60
```

Choose a cap appropriate to the scene and display refresh, and verify the effective settings in the packaged application. These values are examples, not universal defaults.

## Add the app through the UI

In **Apps**, choose **Command** and enter this single line in **URI or command**:

```text
/opt/arena/Arena/Binaries/Linux/Arena-Linux-Shipping Arena -vulkan -sdlvideodriver=wayland -windowed -ResX=1920 -ResY=1143 -ForceRes -unattended -nosplash
```

Replace the path, `Arena` project argument (matching the packaged launcher), and dimensions. Use Suede's source-canvas dimensions, rather than those of one physical output. `-vulkan` selects Vulkan, `-sdlvideodriver=wayland` selects native Wayland, and `-ForceRes` preserves the requested resolution. The remaining flags request a window and suppress interactive startup dialogs and the splash. Save the app, then **Activate** it.

Launch the game binary directly so Suede can match its PID to its window. If a wrapper is necessary, it must replace itself with the game using `exec`. Add the executable's file name to the daemon's `allowed_programs` setting and restart the daemon; the default allowlist covers browser binaries. See
[Allowed programs](configuration.md#allowed-programs).

The UI splits the command on literal spaces and does not interpret shell quoting. Keep the executable path and every argument free of spaces. For space-containing arguments, use an `args` array in the [raw launcher configuration](configuration.md#launchers), or an executable wrapper, and avoid resaving that command through the UI field. In particular, `-ExecCmds` console overrides containing spaces cannot safely go in this field; prefer packaged settings. Allowlist the wrapper's file name if using one.

## Why plain Wayland is the default

UE 5.8.3 Electric Dreams PCGCloseRange at a 1920 × 1143 source canvas was tested using Linux Vulkan on Ubuntu 26.04 and the Quadro RTX 8000 in [System A](developer/test-systems.md#system-a). Checks covered native Wayland capture, four-output warp/blend, process restart, a looping camera, and OSD suppression. Performance trials used three repetitions per mode, 60-second warmups and telemetry windows, and matched CSV camera-frame ranges. The tour repeated camera positions by tick; it was not a deterministic engine replay.

| Combined input/output path | Mean presented FPS |
| --- | ---: |
| Ordinary window / Wayland | 38.30 |
| Retained shared texture / direct output | 37.10 |
| Credit-paced shared texture / direct output | 19.60 |

Retained sharing reduced UE CPU use by 18.6%, but did not improve throughput. At a 20 FPS target, an ordinary frame cap reached the target without the extra throughput loss of the custom credit pacing policy. Plain Wayland therefore provides the simplest integration and the best measured throughput for this scene and hardware.

A separate control measured 39.30 engine FPS with four-output Suede projection, 48.97 in a standalone Wayland window, and 49.80 offscreen without export. Removing Suede improved engine FPS by about 25%, but also removed its projection work; this does not establish an equivalent end-to-end replacement or isolate
a copy-path saving. Offscreen alone added only about 1.7% over the standalone window. These results do not predict ArenaFX performance or establish a browser-versus-UE ranking. Different content, GPUs, drivers, and output layouts need their own measurements; production audio was not qualified by these tests.
