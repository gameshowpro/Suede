# Getting started

## Requirements

| Component | Requirement |
|---|---|
| OS | Debian-family Linux: Ubuntu 22.04 LTS or newer, Debian 13 (Trixie), Raspberry Pi OS Trixie |
| Architecture | x86-64 or aarch64 |
| Compositor | Sway 1.7 or newer, though only 1.10 and 1.11 have been exercised (tearing control needs 1.10). Debian 12 and Raspberry Pi OS Bookworm ship 1.7 and are out of scope for that reason |
| Audio | PipeWire with `pipewire-pulse` |
| Privileges | The daemon runs as the session user, in the `audio`, `video` and `render` groups; provisioning needs sudo once and arranges all of this |

!!! warning "What has actually been tested"
    That table is what Suede is *built* for. What it has been *run* on is two
    machines. Ubuntu 26.04 on x86-64 — Sway 1.11, an NVIDIA GPU with the
    proprietary driver, two DisplayPort outputs, Chromium — where the package
    and the provisioning script have been installed from nothing, upgraded in
    place, and reset back to nothing again. And a Debian 13 (Trixie) x86-64
    testbench — Sway 1.10.1, also NVIDIA proprietary, Chromium — provisioned
    from a fresh headless install. The aarch64 package installs and runs
    under emulation.

    Not yet exercised anywhere: real aarch64 or Raspberry Pi hardware, Sway
    older than 1.10, more than two displays, and the `firefox-kiosk`
    launcher. None of that is expected to be broken; none of it is known to
    work. Reports from any of those are the most useful thing you could send.

## Install

=== "From a release package"

    Download the `.deb` for your architecture from the [releases page](https://github.com/gameshowpro/Suede/releases), then:

    ```bash
    sudo apt install ./suede_*_arm64.deb
    ```

    `apt` pulls in `sway`, `swayidle`, `pipewire`, and `pipewire-pulse` automatically. It does **not** pull in a browser: the package declares no relationship to one, because Suede resolves whichever it finds at launch rather than linking against any. Install one yourself.

    ```bash
    sudo apt install chromium        # Debian, Raspberry Pi OS
    ```

    On Ubuntu that command installs a **snap**, which Suede ignores — it updates itself on its own schedule and restarts the browser when it does, which on an appliance blanks the screens mid-show. Install Google Chrome's own `.deb` there instead, or accept the snap deliberately with `launcher.program` on the application. The `browsers` health check says which it found and which it declined.

=== "From source"

    ```bash
    git clone https://github.com/gameshowpro/Suede.git
    cd Suede
    cargo build --release          # binary at target/release/suede
    cargo install cargo-deb && cargo deb   # or build an installable package
    ```

    Suede has no native library dependencies, so there is nothing to install beyond a Rust toolchain.

## Provision the machine {: #service }

The daemon deliberately does not reconfigure your machine behind your back. The root-level work — auto-login, starting Sway at boot, disabling competing desktop environments — is a separate, explicit step:

```bash
sudo /usr/share/suede/provision.sh
```

It is idempotent, so re-running it after an upgrade is safe. It will:

1. Install any missing required packages, and report which browser it found.
2. Add the appliance user to the `audio`, `video` and `render` groups. Without those, PipeWire cannot open the sound devices and there is no audio at all — an appliance has no seated login, so the ACLs that normally grant access are never applied.
3. Configure auto-login on `tty1` for the appliance user.
4. Start Sway from that user's login shell, adding `--unsupported-gpu` when the NVIDIA proprietary driver is loaded, since Sway will not start on it otherwise.
5. Create a minimal Sway config, including the block Suede manages.
6. Install `sway-session.target` and enable `suede.service`.
7. Disable and mask competing display managers and compositors.
8. Open port 9088 in `ufw` or `firewalld`, if one is active. Pass `--no-firewall` to skip this, or `--port N` if you have moved the API.

It warns rather than proceeds silently in two cases worth knowing about: if
something else on the machine already starts Sway — two compositors cannot
share a graphics card, and whichever publishes `SWAYSOCK` last is the one
Suede attaches to — and if it changed your group membership, since that only
takes effect after a reboot.

!!! note "Raspberry Pi OS"
    Pi OS ships `labwc`, which will fight Sway for the displays. The provisioning script disables it, including its autostart entry. This path is written but untested — see the warning above.

Reboot when it finishes. The machine will log in, start Sway, and start Suede.

## First run

Open `http://<machine>:9088/` from another computer on the network. If it times out rather than refusing, a host firewall is dropping the port — see [Network access](#network-access); on Ubuntu Server it usually is.

The web UI shows a banner for any health check that is not passing, with a **Fix this** button where Suede can safely remediate the problem itself. Work through those first — they cover the handful of things provisioning cannot do, such as enabling the user service before the first login has happened.

Then:

1. Go to **Displays**. Every connector the machine has is listed, including ones with nothing plugged in. Add the ones you want to the layout, set each one's mode and position, and save.
2. Go to **Apps**. Add a Chromium kiosk pointing at the URI you want shown, then activate it. One application is active at a time and it covers every display as a single canvas — there is no per-display targeting. The dialog shows the command, arguments and environment it will actually be launched with, resolved on this machine.
3. Reboot the machine to prove it comes back on its own.

That last step is the point of the whole exercise; do it once before you leave the site.

## Sway configuration {: #sway-configuration }

Suede owns a marker-delimited block in `~/.config/sway/config`:

```
# BEGIN SUEDE_CONFIG
...
# END SUEDE_CONFIG
```

Everything outside those markers is yours and is preserved byte for byte. The block hands the session environment to systemd (so the user service can find `SWAYSOCK`) and starts `sway-session.target`.

If the block is missing, the `sway-config` health check offers to add it.

!!! warning "Do not configure outputs in the Sway config"
    Suede is the single source of truth for output configuration. Declaring `output` blocks in the Sway config as well will produce a fight between the two on every reload.

## Browsers {: #browsers }

Suede launches browsers as its own child processes, not through Sway's `exec`. That is what gives it a real PID for clean termination, exit-code observation, and restart policies.

Chromium refuses to start a second instance sharing a profile, so each app automatically gets a private `--user-data-dir` beneath the state directory. Profiles are wiped on launch by default; set `persistProfile: true` on an app to keep it.

The exception is a browser named explicitly as a snap, whose profile goes under `~/snap/<name>/common/` instead — a confined snap may write anywhere in `$HOME` except a hidden directory, and the state directory is under `~/.local/state`. The app dialog shows where the profile will land, so this is visible rather than surprising.

## Audio {: #audio }

Audio is outside Sway's scope, so Suede talks to PipeWire directly through `pw-dump` and `pw-cli`. Sinks are identified by their PipeWire `node.name`, which is derived from the hardware path and is stable across reboots and replugging.

Per-application routing is applied at launch through `PULSE_SINK`, which means changing an app's sink relaunches it. Routing an app to `null` sends it to a Suede-managed silent sink, so the browser still sees a working audio device.

Check that `pipewire-pulse` is actually running — the `pipewire` health check covers this. Without it, browsers have no audio device at all.

## Network access {: #network-access }

Suede binds `0.0.0.0:9088` by default, so a freshly provisioned appliance is reachable from the network it is on. Read [Security posture](#security-posture) — that default is also unauthenticated.

To restrict it to the machine itself:

```toml
# ~/.config/suede/suede.toml
bind = "127.0.0.1:9088"
```

or pass `--bind 127.0.0.1:9088`. You can then still reach the UI by forwarding the port:

```bash
ssh -L 9088:127.0.0.1:9088 appliance
# then open http://localhost:9088/
```

### When it will not connect

A host firewall is the usual culprit, and its signature is distinctive: the connection **times out** rather than being refused, because the default policy drops packets instead of rejecting them. From the outside that is indistinguishable from a machine that is switched off — and every health check still passes, because they all run from inside.

Ubuntu Server enables `ufw` with only SSH allowed, so a default install blocks Suede even though Suede is listening correctly. This is the common case:

```bash
sudo ufw allow 9088/tcp
# or, scoped to the network the appliance serves:
sudo ufw allow from 192.168.1.0/24 to any port 9088 proto tcp
```

On a `firewalld` host:

```bash
sudo firewall-cmd --permanent --add-port=9088/tcp && sudo firewall-cmd --reload
```

The `api-reachability` check reports which of these applies. Suede can detect that a filter is *running* but not read its rules — `/etc/ufw/user.rules` is root-only, and the session user is deliberately not root — so it warns and names the command rather than claiming to know whether the port is open. It cannot test the port itself either: traffic from the appliance to its own address never crosses the filter, so a self-test would succeed no matter what.

What it does instead is watch for evidence. The moment a request arrives from an address that is not this machine, the port is proven open and the check passes, naming the client. So the warning clears itself the first time you load the page from your laptop — which matters, because a warning that can never be cleared is one people learn to scroll past.

Two things it cannot see, worth ruling out if the command above does not help: a filter on a router between you and the appliance, and container runtimes such as Docker, which insert their own rules ahead of `ufw`'s.

## Security posture

Suede is **unauthenticated by default**. The expected deployment is a trusted production LAN, and a web UI served from the same machine would have to hand its credential to every user anyway.

For less trusted networks, set a bearer token:

```toml
# ~/.config/suede/suede.toml
token = "a-long-random-string"
```

or `SUEDE_TOKEN` in the environment. When a token is configured:

- Every endpoint requires `Authorization: Bearer <token>`, except `/healthz` and the loopback-only heartbeat endpoint.
- **The web UI and the `/docs` page are disabled**, because a page that embedded the token would defeat it. Token mode is for machine-to-machine clients.

There is no TLS. Front Suede with a reverse proxy where transport security matters.
