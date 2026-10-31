# Docs screenshots

`scripts/capture-screenshots.sh` retakes the Displays tab captures used in the
docs from a live appliance, so a UI change can be followed by a fresh set
instead of a hand-made one.

```sh
scripts/capture-screenshots.sh http://<appliance>:9088 healthy
scripts/capture-screenshots.sh http://<appliance>:9088 disconnected
```

Each run:

1. Opens the web UI in headless Chromium in the dark theme at 100% scale. The
   viewport width is the narrowest at which the page shows two columns, read
   from the page's stylesheet, so the captures follow the layout if that
   breakpoint moves.
2. Selects the first output and takes the captures listed for the phase in
   `SHOTS` in `capture.cjs`: full pages, crops around named elements, and the
   **Arrange automatically** dialog in each mode.
3. Puts the appliance's working copy back exactly as it was. The healthy phase
   switches the mode toggle, which edits the working copy. Unsaved edits that
   were there before are rewritten, and a clean working copy is reverted. The
   result is compared with a backup and any difference is an error.
4. Encodes every PNG to AVIF (libaom, 4:4:4, CRF 40) under
   `docs/assets/screenshots/`, and updates the `width` and `height`
   attributes of every docs image that shows it.

To retake only some of a phase's captures, list their names in
`SCREENSHOT_ONLY`, separated by commas:

```sh
SCREENSHOT_ONLY=details/warp-editor scripts/capture-screenshots.sh http://<appliance>:9088 healthy
```

The PNGs and the working-copy backup stay in `$SCREENSHOT_OUT` (a new
temporary directory by default) for review. Look at every capture before
committing it.

## Phases

- **healthy:** every output connected and the slicer running. Takes
  `displays`, the Displays-tab crops and both arrange dialogs.
- **disconnected:** every output unplugged. Takes `displays-disconnected`, the
  diagram and outputs table, and the warning banners.

## Redaction

The docs must not show real machine names or private project names. Before
every capture the page's text, tooltips and field values are rewritten, and a
capture that still shows a match fails the run. The rules come from two local
files that are not in the repository:

- the alias table at the top of `.claude/test-beds.md`: each machine name is
  replaced by its alias in hostname form (`system-a`);
- `.claude/screenshot-redactions.tsv`: one rule per line, a case-insensitive
  regular expression, a tab and the replacement. `#` starts a comment. Rules
  apply in order and only where the match does not touch other letters, so put
  identifier forms (such as a name followed by `-`) before the plain forms.

Without the redaction file the script refuses to run, unless
`SCREENSHOT_ALLOW_UNREDACTED=1` is set. Text drawn into a canvas or an image
cannot be rewritten or checked, so review those parts by eye.

## Requirements

- Playwright and its Chromium, installed as in [`tests/ui/README.md`](../../tests/ui/README.md).
  `NODE_PATH` and `PLAYWRIGHT_BROWSERS_PATH` default to that location.
- `ffmpeg` and `ffprobe` with `libaom-av1`.
- Network access to the appliance's API port.

## Changing what is captured

Edit `SHOTS` in `capture.cjs`. A shot names its output file (relative to
`docs/assets/screenshots/`, without the extension), the projection mode it
needs, and either `full: true`, `dialog: true` or `el` (CSS selectors whose
union is the crop) with `pad` pixels around it. `pins` sets destination pins in
the warp editor for that capture only, so it shows a warped picture. A new image still has to be
placed in the docs by hand the first time; after that the script keeps its size
attributes current.
