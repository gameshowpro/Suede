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
