# Getting started

## Requirements

| Component | Requirement |
|---|---|
| OS | Debian-family Linux (Debian 12+, Raspberry Pi OS Bookworm+) |
| Architecture | x86-64 or aarch64 |
| Compositor | Sway 1.7 or newer (tearing control needs 1.10) |
| Audio | PipeWire with `pipewire-pulse` |
| Privileges | The daemon runs as the session user; provisioning needs sudo once |

## Install

=== "From a release package"

    Download the `.deb` for your architecture from the [releases page](https://github.com/gameshowpro/Suede/releases), then:

    ```bash
    sudo apt install ./suede_*_arm64.deb
    ```

    `apt` pulls in `sway`, `pipewire`, and `pipewire-pulse` automatically. A browser is *recommended* rather than required, so install one explicitly if the machine has none:

    ```bash
    sudo apt install chromium
    ```

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

1. Install any missing required packages.
2. Configure auto-login on `tty1` for the appliance user.
3. Start Sway from that user's login shell.
4. Create a minimal Sway config, including the block Suede manages.
5. Install `sway-session.target` and enable `suede.service`.
6. Disable and mask competing display managers and compositors.
7. Open port 7071 in `ufw` or `firewalld`, if one is active. Pass `--no-firewall` to skip this, or `--port N` if you have moved the API.

!!! note "Raspberry Pi OS"
    Pi OS ships `labwc`, which will fight Sway for the displays. The provisioning script disables it, including its autostart entry.

Reboot when it finishes. The machine will log in, start Sway, and start Suede.

## First run

Open `http://<machine>:7071/` from another computer on the network. If it times out rather than refusing, a host firewall is dropping the port — see [Network access](#network-access); on Ubuntu Server it usually is.

The web UI shows a banner for any health check that is not passing, with a **Fix this** button where Suede can safely remediate the problem itself. Work through those first — they cover the handful of things provisioning cannot do, such as enabling the user service before the first login has happened.

Then:

1. Go to **Displays**. Click a display, set its mode and position, and save.
2. Go to **Apps**. Add a Chromium kiosk pointing at the URI you want shown, pinned to that display.
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

## Audio {: #audio }

Audio is outside Sway's scope, so Suede talks to PipeWire directly through `pw-dump` and `pw-cli`. Sinks are identified by their PipeWire `node.name`, which is derived from the hardware path and is stable across reboots and replugging.

Per-application routing is applied at launch through `PULSE_SINK`, which means changing an app's sink relaunches it. Routing an app to `null` sends it to a Suede-managed silent sink, so the browser still sees a working audio device.

Check that `pipewire-pulse` is actually running — the `pipewire` health check covers this. Without it, browsers have no audio device at all.

## Network access {: #network-access }

Suede binds `0.0.0.0:7071` by default, so a freshly provisioned appliance is reachable from the network it is on. Read [Security posture](#security-posture) — that default is also unauthenticated.

To restrict it to the machine itself:

```toml
# ~/.config/suede/suede.toml
bind = "127.0.0.1:7071"
```

or pass `--bind 127.0.0.1:7071`. You can then still reach the UI by forwarding the port:

```bash
ssh -L 7071:127.0.0.1:7071 appliance
# then open http://localhost:7071/
```

### When it will not connect

A host firewall is the usual culprit, and its signature is distinctive: the connection **times out** rather than being refused, because the default policy drops packets instead of rejecting them. From the outside that is indistinguishable from a machine that is switched off — and every health check still passes, because they all run from inside.

Ubuntu Server enables `ufw` with only SSH allowed, so a default install blocks Suede even though Suede is listening correctly. This is the common case:

```bash
sudo ufw allow 7071/tcp
# or, scoped to the network the appliance serves:
sudo ufw allow from 192.168.1.0/24 to any port 7071 proto tcp
```

On a `firewalld` host:

```bash
sudo firewall-cmd --permanent --add-port=7071/tcp && sudo firewall-cmd --reload
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
