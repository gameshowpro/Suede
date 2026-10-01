# Deployment

## Packaging

A Rust release build is a single self-contained ELF binary whose only dynamic dependencies are libc — there is no runtime to install. Packaging is therefore just placing one file plus its service plumbing, which is what `cargo-deb` formalizes. All of it is configured in `Cargo.toml` under `[package.metadata.deb]`; there is no separate packaging tree to keep in step.

| Path | Contents |
|---|---|
| `/usr/bin/suede` | The binary, with the web UI embedded |
| `/usr/lib/systemd/user/suede.service` | The systemd **user** unit |
| `/usr/share/suede/provision.sh` | Root provisioning script |
| `/usr/share/doc/suede/examples/` | Bootstrap config (`suede.toml`) and four desired-state examples: a plain tiled appliance, and warp, shared-canvas, and adaptive-black-lift projection layouts |

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

`postinst` also grants `/usr/bin/suede` the `cap_sys_nice+ep` file capability (via `setcap`, from the `libcap2-bin` Recommends) so the slicer's GPU blend can negotiate a realtime Vulkan queue priority instead of the driver's unprivileged "medium" default — see [Where the blend runs](../how-it-works.md#where-the-blend-runs). dpkg does not preserve file capabilities across an upgrade, so this reapplies on every `configure`, not just first install.

The package also `Recommends` `hwdata`: for experimental direct presentation, the daemon names each display it drives itself from its EDID, and `/usr/share/hwdata/pnp.ids` is the same PNP vendor table wlroots compiles in, so the names it produces agree with what Sway reports for the same display. Without it the daemon falls back to systemd's `20-acpi-vendor.hwdb` and then to the bare three-letter code — worth a `Recommends`, not worth refusing an install over, since Wayland needs none of it.

### The login profile block {: #the-login-profile-block }

`provision.sh` writes a block into the tty1 auto-login user's `~/.bash_profile`, between `# BEGIN SUEDE_PROVISION` and `# END SUEDE_PROVISION` markers so re-running it replaces only its own block. Two pieces of it are generated from their own heredocs so `scripts/validate-packaging.sh` can lift each one out and run it against sample `suede.toml` files in isolation:

- **The scanout block** (`SCANOUT_EOF`) derives `WLR_SCENE_DISABLE_DIRECT_SCANOUT` from `allow_overlaps` and `direct_scanout` every time the profile runs — see [Overlapping layouts and direct scanout](../configuration.md#direct-scanout).
- **The presentation block** (`PRESENTATION_EOF`), for [experimental direct presentation](../configuration.md#experimental-direct-presentation): reads `presentation` and `allow_overlaps` from `suede.toml`, decides whether this login starts a headless-only Sway or the ordinary DRM one, and exports `WLR_BACKENDS=headless`, `WLR_HEADLESS_OUTPUTS=1` and `WLR_RENDER_DRM_DEVICE` for the former. Runtime state lives in `$XDG_RUNTIME_DIR/suede`, a tmpfs, so it lasts only for the current boot:

  | File | Holds |
  |---|---|
  | `session` | what the last login started, `direct` or `wayland` |
  | `direct-attempts` | headless starts this boot the daemon has not yet confirmed; the third one falls back |
  | `presentation-fallback` | JSON `{reason, time, bootId}` once a fallback has happened; present means `wayland` for the rest of this boot |

  Before starting anything, the block also waits up to 15 seconds for a
  previous direct session's `suede slice` process to release DRM master, then
  runs `suede display-reset` — see [the recovery
  runbook](vk-khr.md#recovery-runbook) — so an operator restarting
  `getty@tty1` mid-session cannot race the DRM Sway onto a card the slicer
  still holds.

  `WLR_RENDER_DRM_DEVICE` is not simply the first `/dev/dri/renderD*` found:
  on a machine with more than one GPU — an integrated GPU beside the discrete
  card that actually drives the wall, [System B](test-systems.md#system-b)'s
  shape — that would allocate the headless canvas on the wrong device. The
  block instead scans `/sys/class/drm/card*-*/status` for the card with the
  most connected connectors (never one with none, even if it is enumerated
  first) and picks that card's own render node, falling back to the first
  `renderD*` found at all only when no card reports a connected connector.
  This has to agree with the daemon's own card choice
  (`DrmInventory::preflight` (`src/drm_inventory.rs`)) and with
  `pick_direct_physical_device`'s DRM-primary match
  (`src/projection/gpu/display.rs`),
  or the slicer starts against a GPU with no path to the displays.
  `scripts/validate-packaging.sh` exercises the rule against a fake sysfs
  tree via the `SUEDE_DRM_SYSFS`/`SUEDE_DRM_DEV` overrides the block reads
  instead of the real filesystem when they are set.

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
hide behind whatever the last run left behind. `reset-machine.sh` returns a
machine to the state it was in before it met Suede.

It ships in the package, next to `provision.sh`, so an appliance already has
it and there is nothing to copy over:

```bash
sudo /usr/share/suede/reset-machine.sh --user hamish --dry-run  # change nothing
sudo /usr/share/suede/reset-machine.sh --user hamish
sudo reboot                                                     # see below
```

From a checkout — on a machine where the package was never installed, or to
run a version newer than the installed one — it is `packaging/reset-machine.sh`
relative to the repository root:

```bash
sudo packaging/reset-machine.sh --user hamish --dry-run
```

Running the installed copy removes the package underneath itself, which is
fine: the shell holds the file open and reads it to the end regardless of the
directory entry disappearing. That is verified rather than assumed.

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
| `build` | push to `main`, `research/**` or `preview/**` | Release binaries and `.deb`s for amd64 and arm64 |
| `release` | push to main | Tags and publishes if the version is new |
| `build-docs` | docs changes on main | Generates API assets, builds the site with `--strict` |
| `deploy-docs` | after build-docs | Publishes to GitHub Pages |

The `build` job asserts the binary is self-contained by checking `ldd` output — anything beyond libc means a native dependency crept in and cross-compilation is about to get much harder.

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
