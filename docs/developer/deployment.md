# Deployment

## Packaging

A Rust release build is a single self-contained ELF binary whose only dynamic dependencies are libc — there is no runtime to install. Packaging is therefore just placing one file plus its service plumbing, which is what `cargo-deb` formalizes. All of it is configured in `Cargo.toml` under `[package.metadata.deb]`; there is no separate packaging tree to keep in step.

| Path | Contents |
|---|---|
| `/usr/bin/suede` | The binary, with the web UI embedded |
| `/usr/lib/systemd/user/suede.service` | The systemd **user** unit |
| `/usr/share/suede/provision.sh` | Root provisioning script |
| `/usr/share/doc/suede/examples/` | Bootstrap config and a four-output example |

Declared dependencies are `sway`, `pipewire`, and `pipewire-pulse`, with `chromium | firefox` recommended.

```bash
cargo deb                                   # host architecture
cross build --release --target aarch64-unknown-linux-gnu
cargo deb --no-build --target aarch64-unknown-linux-gnu
```

Cross-compilation stays trivial precisely because there are no native library dependencies — the reason PipeWire is driven through its CLI tools rather than its C library.

## Install and upgrade

```bash
# First install
sudo apt install ./suede_1.2.3-1_arm64.deb
sudo /usr/share/suede/provision.sh
sudo reboot

# Upgrade
sudo apt install ./suede_1.2.4-1_arm64.deb
```

`postinst` reloads unit definitions and restarts a running instance; it never enables anything or edits a user's session. Desired state lives in `$XDG_STATE_HOME` and is untouched by package operations, so configuration survives upgrades by construction. Re-running `provision.sh` is safe but only needed when the provisioning itself changed.

The package declares **no relationship to a browser**. Suede resolves whichever
of `chromium`, `chromium-browser`, `google-chrome-stable`, `google-chrome`,
`firefox` or `firefox-esr` it finds when it launches an app, and both
provisioning and the `browsers` health check say plainly when there is none.
Declaring them would claim a coupling that does not exist, and the names are
wrong somewhere whichever list you pick: Debian has a real `chromium` package,
Ubuntu has a transitional one that installs a snap, and `google-chrome-stable`
is in no distribution's archive at all. As a `Recommends` it did real harm —
apt installs those by default, so a plain install pulled snapd and a snap
Chromium onto an appliance that already had a browser.

## Testing an install honestly

An installer tested on a machine that has already been installed on proves
very little: the failures worth finding only happen the first time, and they
hide behind whatever the last run left behind. `scripts/reset-machine.sh`
returns a machine to the state it was in before it met Suede.

```bash
sudo ./scripts/reset-machine.sh --user hamish --dry-run   # list, change nothing
sudo ./scripts/reset-machine.sh --user hamish
sudo reboot                                               # see below
```

It removes the package (or a binary built and copied into place), the
per-user configuration and state, and the changes provisioning made: the
tty1 auto-login drop-in, its block in `~/.bash_profile`, the daemon's block
in the sway config, and `sway-session.target`. It stops the daemon, and also
the slicer, blend overlays and kiosk browsers, which outlive it.

It deliberately leaves alone anything it cannot safely claim as its own:
sway, PipeWire and browsers are ordinary packages that were probably wanted
anyway; a compositor unit you wrote yourself is reported rather than deleted;
and masked display managers stay masked unless `--restore-desktop` is passed,
because re-enabling one on a machine already running a compositor is its own
kind of mess. `--keep-state` preserves the desired-state document and browser
profiles.

**Reboot after resetting.** Group membership and the compositor are
inherited from login, not re-read, so a session that has already seen Suede
carries some of it into the next test regardless of what is on disk.

## Releasing

`Cargo.toml`'s `version` is the single source of truth. On a push to `main`, CI builds both architectures, and if the version is new, tags `v{version}` and creates a GitHub release with both `.deb`s attached.

To cut a release: bump the version in `Cargo.toml`, merge to `main`. Nothing else.

## CI

One workflow, `ci.yml`, with `dorny/paths-filter` splitting app changes from docs changes and a `docs_only` dispatch input for redeploying the site without rebuilding.

| Job | Runs when | Does |
|---|---|---|
| `test` | app changes, all PRs | fmt, clippy, tests, OpenAPI generation, smoke test |
| `build` | push to main | Release binaries and `.deb`s for amd64 and arm64 |
| `release` | push to main | Tags and publishes if the version is new |
| `build-docs` | docs changes on main | Generates API assets, builds the site with `--strict` |
| `deploy-docs` | after build-docs | Publishes to GitHub Pages |

The `test` job asserts the binary is self-contained by checking `ldd` output — anything beyond libc means a native dependency crept in and cross-compilation is about to get much harder.

## The documentation site

Built with MkDocs and the Material theme. The API reference is generated from the code being deployed:

```bash
scripts/build-docs.sh   # suede openapi → docs/generated/openapi.json, vendors Scalar
mkdocs serve
```

`suede openapi` needs neither a compositor nor a network, which is what makes this possible in CI. The Scalar bundle is vendored into `docs/generated/` at build time, so the published page makes no CDN requests when viewed. Both are git-ignored; they are derived artifacts.

A snapshot test (`tests/openapi_snapshot.rs`) guards the document, so an endpoint or DTO change shows up as a reviewable diff rather than silently altering the published reference.

## On-device verification

Some things only a real appliance can prove. Run this checklist once on real hardware before trusting a release:

- [ ] Four displays enumerate with correct EDID make/model and mode lists.
- [ ] Setting a mode, position, and scale on each takes effect.
- [ ] Four Chromium kiosks launch, one per output, each fullscreen on the right display.
- [ ] Audio from each browser reaches its configured sink; a null-routed app is silent.
- [ ] Unplugging a display moves status to `degraded`; replugging returns it to `synced` and relaunches its app.
- [ ] `kill -9` on a browser relaunches it within the backoff window.
- [ ] A page that stops posting heartbeats is relaunched.
- [ ] A cold reboot restores everything with no operator action.
- [ ] `systemctl --user stop suede` leaves no orphaned browser processes.
- [ ] The cursor is invisible and parked off every display.
