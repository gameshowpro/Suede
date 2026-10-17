# Suede artwork

This directory contains the definitive artwork. Edit these sources rather than
any generated files:

| Source | Purpose |
| --- | --- |
| `suede.png` | Approved blue suede panels, native 1254 × 1254 RGBA, with a transparent background and drop shadow. Never upscale this source. |
| `suede.svg` | Editable vector interpretation with procedural suede texture and shadow. It approximates the PNG rather than embedding it. |
| `suede-small.svg` | Optically simplified source for small icons: larger panels, clear gaps, no texture or shadow. |
| `suede-mono.svg` | Monochrome source with the wave cut out of the panel silhouettes. |
| `gameshowpro-light.svg` | Game Show Pro logo for light backgrounds; the maintainer's brand asset, not generated from the Suede sources. The About box inlines a copy whose "Pro" lettering uses `currentColor` so it follows the theme; this file stays unmodified. |

The two specialized SVGs are maintained design sources, not resized exports.
When changing the identity, update these deliberate small-size and monochrome
adaptations as well as the master artwork. The PNG originated from the approved
Upright Inlay concept and background-removal edit using the built-in image
generation tool; its native resolution is retained unchanged.

## Generation

The shared generator is the Rust package in `scripts/artwork/`. It runs on the
build host, so ordinary and cross-compiled builds need no image CLI, Python,
Node, or system graphics libraries. Dependencies are locked by the root
`Cargo.lock`.

- `cargo build` generates and embeds web assets in Cargo's `OUT_DIR/artwork/`.
  `build.rs` tracks the artwork and generator inputs. The application serves
  the embedded files at `/assets/artwork/` and the conventional `/favicon.ico`.
- `cargo run --locked -p suede-artwork -- docs` generates documentation assets
  into `docs/generated/artwork/`.
- `mkdocs build` and `mkdocs serve` invoke the same command through
  `scripts/artwork_hook.py`; run `scripts/build-docs.sh` first to prepare the
  site's OpenAPI and Scalar assets, as usual.
  The hook caches source and output hashes, so text-only edits reuse the icons
  while changed or missing derivatives are regenerated.
- `cargo run --locked -p suede-artwork -- web /tmp/suede-artwork` exports the
  application suite for inspection without building the daemon.

| Consumer | Generated file | Pixel size | Display size |
| --- | --- | --- | --- |
| Application header | `icon-header.avif` / `.png` | 144 × 144 | 48 × 48 CSS px (40 × 40 on smaller screens) |
| MkDocs header | `icon-docs.avif` / `.png` | 144 × 144 | 48 × 48 CSS px (40 × 40 on smaller screens) |
| Application About box | `icon-readme.avif` / `.png` | 384 × 384 | 128 × 128 CSS px |
| Documentation home | `icon-readme.avif` / `.png` | 384 × 384 | 128 × 128 CSS px |
| GitHub README | `icon-readme.png` | 384 × 384 | 128 × 128 CSS px |
| Social preview cards | `social-card.png` | 1200 × 630 | Determined by the consuming service |

Header and home images include three pixels per CSS pixel for high-density
displays. The generator trims nearly invisible alpha specks for framing while
preserving the visible shadow; it never resamples raster artwork upward.
Each header and home derivative also has an AVIF export using ravif at color
quality 80, 4:4:4 chroma sampling, alpha quality 100, and speed 6. Alpha is
encoded at maximum quality, but the encoder does not guarantee losslessness.
Web markup prefers AVIF through `<picture>`
and falls back to the matching PNG for browsers without AVIF support. The
canonical `suede.png` remains lossless and unchanged; compatibility formats
such as PNG and the existing favicon, touch-icon, and vector exports are
unchanged.
Small favicons are rendered from vector geometry. Home screen and maskable
icons use opaque square backgrounds where appropriate. Maskable artwork fits
inside the central safe circle rather than relying on a baked-in rounded edge.

Only these canonical sources and the generation code belong in Git. Cargo
output, `docs/generated/`, `site/`, and exploratory `output/` files are ignored.
The README references the published documentation image at
`https://suede.gameshow.pro/generated/artwork/icon-readme.png` so GitHub can
display the generated version without committing it. That URL updates when
the documentation is deployed; a local README preview needs network access.

## Validation

Run `cargo test -p suede-artwork` for export checks and
`cargo test -p suede --lib api::ui::tests` for embedded asset routing. The normal
workspace CI checks include the generator. Build documentation with
`mkdocs build --strict` to validate the theme and page references.

Reference conventions:
[web app icons](https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest/Reference/icons),
[maskable safe areas](https://web.dev/articles/maskable-icon),
[Apple touch icons](https://developer.apple.com/library/archive/documentation/AppleApplications/Reference/SafariWebContent/ConfiguringWebApplications/ConfiguringWebApplications.html),
and [Safari pinned tabs](https://developer.apple.com/library/archive/documentation/AppleApplications/Reference/SafariWebContent/pinnedTabs/pinnedTabs.html).
