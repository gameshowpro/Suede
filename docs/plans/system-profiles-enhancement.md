# System Profiles: app subsets, selective activation, device mapping

## Original request

When saving a system profile, one of the options is to save the apps. Enhance this so there's a button to open a popup to select a subset of the currently available apps. When the dialog is closed, the `apps` checkbox will show unchecked (`none`), checked (`all`), or the indeterminate state (`subset`). Click that box allows the use to cycle between to `all` and `none` again.

The activate dialog box should have a similar scheme, allowing the user to select which of the stored objects to activate, including `none`, `all`, or `subset` of the stored apps. Of course, any part that is not available in the profile should be disabled. 

The current feature is the activate dialog is not very clear. I guess it's giving the user the opportunity to map the display names currently available to the system. It should be a combo box where the user can freely type of select one of the detected devices from the list. To apply the defaults when the dialog opens, first select any real devices with a perfect named match to the profile device. The go through every assignment without a default already, giving each one the lowest-indexed device not already assigned in this dialog.

---

## Specification

Status: proposed, not started. Everything below is grounded in the code as of
commit `a003911`. Line numbers are approximate and will drift; the identifiers
will not.

### 1. Where the feature stands today

**Data model** (`src/profiles.rs`):

- `ProfileIncludes` (line 23) has five booleans: `displays`, `backgrounds`,
  `apps`, `activeApp`, `settings`. These are the only "parts" a profile has.
  `displays` covers `config.outputs` and `config.projection`; `backgrounds`
  covers `config.backgrounds` and the wallpaper bytes in `assets`; `apps`
  covers `config.apps`; `activeApp` covers `config.activeApp`; `settings`
  covers `config.settings`. It is `deny_unknown_fields`, so a stored or
  imported profile with an extra key in `includes` is rejected today.
- `SystemProfile` (line 45): `formatVersion` (always `1`), `name`,
  `description`, `includes`, `config` (a full `DesiredState`), `assets`.
- `SystemProfile::normalize` (line 60) clears every section whose flag is
  false, so a stored profile never carries data for a part it does not
  include. `SystemProfile::merge` (line 170) copies each included section over
  the live document wholesale: with `includes.apps == true`, `merged.apps =
  self.config.apps.clone()`, so an included empty app list empties the live
  catalog (documented in `docs/configuration.md`, tested in
  `included_empty_apps_replaces_current_catalog_and_missing_active_app_is_invalid`).
- `ProfileSummary` (line 196): `{id, name, description, includes}`, returned by
  `GET /api/v1/profiles`.

**API** (`src/api/profiles.rs`):

- `POST /api/v1/profiles` takes `CaptureProfile {id, name, description,
  includes}` and `PUT /api/v1/profiles/{id}` takes `UpdateProfile {name,
  description, includes}`. Both call `capture()` (line 83), which snapshots
  `state.store.effective()`, sets `includes`, calls `normalize()`, embeds the
  referenced wallpapers, and validates. There is no way to capture a subset of
  the catalog: `apps` is all or nothing.
- `POST /api/v1/profiles/{id}/activate` takes `ActivateProfile
  {outputMappings, audioMappings}` (line 59, `default, deny_unknown_fields`).
  `ActivateProfile::apply` (line 174) validates every mapping source against
  the stored profile (`422` for an unknown source or a blank target), rewrites
  matched `OutputMatch` rules to `OutputMatch::by_name(target)` with `adopted`
  cleared, and rewrites `app.audio.output`. The handler (line 218) then merges
  every included section with `profile.merge(&basis)`. There is no way to
  activate fewer sections than the profile includes.

**Web UI** (`src/api/ui/index.html`):

- Markup: the tab at `#tab-profiles` (line 1053), the import dialog
  `#profile-import-dialog` (line 1068), and the inline editor panel
  `#profile-editor` (line 1082) with five `[data-profile-include]` checkboxes
  in a `<fieldset>` whose legend is "Configuration sections".
- Script: `// --- system profiles ---` (line 4873). `selectedProfileIncludes()`
  reads the five checkboxes as booleans. `openProfileEditor(profile)` opens the
  panel for "Save new profile" or "Update profile snapshot"; `#profile-save`
  flushes pending previews and sends `POST /profiles` or `PUT /profiles/{id}`.
- `activateSystemProfile(profile)` (line 5030) fetches the stored profile,
  builds a `<dialog>` from a template string with one free-text `<input
  id="profile-map-N">` per saved display rule and per distinct saved sink
  name, prefilled by `profileNames()` from the *current configuration's*
  output keys and sink names, not from detected hardware. The only explanation
  is "Map names from this profile to names on this system. Leave a value
  unchanged to keep the same name." A row with no match is left blank. On
  Activate, a mapping is sent only when the field is nonblank and differs from
  the row's key.
- Detected devices already live in page state: `outputs` (from `GET
  /api/v1/outputs`, line 6047) and `sinks` (from `GET /api/v1/av`
  `.audioOutputs`, line 6052), both refreshed by SSE. The app dialog's sink
  picker (line 4612) filters out `isNullSink` sinks and labels a sink
  `description ?? id`.

**Tests**: `src/api/profile_tests.rs` (HTTP behavior through the in-process
router) and `tests/ui/system-profiles.cjs` (Playwright against a fixture that
intercepts every API call). The UI test currently asserts that an unknown
saved display is left blank rather than assigned to the first connected
display; this spec deliberately reverses that.

**Docs**: `docs/configuration.md` "System Profiles" (line 546) is the only
user-facing text. There is no screenshot of the tab.

### 2. Terminology used below

- **Part** or **section**: one of the five `includes` keys. The UI calls them
  sections ("Configuration sections"); keep that word in the UI.
- **Current apps**: `config.apps` of the effective configuration, which is
  what `capture()` snapshots. This is the list the Save dialog's picker shows.
  It is the same id set as `GET /api/v1/apps`, but the picker reads
  `config.apps` because it needs the launcher details for labels.
- **Stored apps**: `profile.config.apps` of the saved profile, which the
  Activate dialog's picker shows.
- **Tri-state**: `none` (checkbox unchecked), `all` (checked), `subset`
  (`indeterminate` property set, `checked` false).
- **Detected device**: for displays, an entry of `GET /api/v1/outputs` (what
  sway reports connected); for audio, an entry of `GET /api/v1/av`
  `.audioOutputs` with `isNullSink == false`.
- **Device index**: the position of a detected device in the array the daemon
  returns, after the null-sink filter for audio. The page never sorts these
  lists; the daemon's order is the definition.

### 3. Data model and API changes

#### 3.1 Principle: the stored format does not change

A profile that contains a subset of apps is simply a profile whose
`includes.apps` is `true` and whose `config.apps` holds only the chosen apps.
Nothing in the stored document records "this was a subset"; the stored list
*is* the selection. Therefore:

- `PROFILE_FORMAT_VERSION` stays `1`.
- `ProfileIncludes` keeps its five booleans. Existing stored profiles and
  exported `.json` files remain valid without migration, and files exported by
  the new build import into the old build.
- `ProfileSummary` is unchanged (see open question 6).

The tri-state is a property of the two *requests* (capture and activate), not
of the profile.

#### 3.2 A shared selection type

Add to `src/profiles.rs`:

```rust
/// A section choice in a request: every item, none, or a named subset.
/// Serialized as `true`, `false`, or an array of ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum SectionSelection {
    Flag(bool),
    Subset(Vec<String>),
}
```

Semantics, applied identically wherever the type appears:

- `true`: everything.
- `false` or `[]`: nothing.
- `["a", "b"]`: exactly those ids. Duplicates are collapsed. Only the `apps`
  section accepts a subset; the other four sections are booleans in both
  requests, so a client cannot send a list for them.

Give it helpers `is_none()`, `is_all()`, and `ids() -> Option<BTreeSet<&str>>`
so the two handlers share the normalization.

#### 3.3 Capture: `POST /api/v1/profiles` and `PUT /api/v1/profiles/{id}`

Replace the `includes: ProfileIncludes` field of `CaptureProfile` and
`UpdateProfile` with `includes: CaptureIncludes`:

```rust
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct CaptureIncludes {
    pub displays: bool,
    pub backgrounds: bool,
    pub apps: SectionSelection,   // default: Flag(true)
    pub active_app: bool,
    pub settings: bool,
}
```

Defaults match `ProfileIncludes::default()` (all true), so a request that
omits `includes`, or sends the old boolean `apps`, behaves exactly as before.

`capture()` becomes:

1. Snapshot `state.store.effective()`.
2. If `includes.apps` is a subset, validate every id against the snapshot's
   `apps`. An unknown id is `422` with
   `app "lobby-loop" is not in the current configuration`. Then retain only
   the selected apps, **in the snapshot's order** (not the request's order),
   so the stored catalog order is stable regardless of how the UI assembled
   the list.
3. Derive `ProfileIncludes { apps: !includes.apps.is_none(), ..the four bools }`.
   A subset of one or more ids stores as `includes.apps: true`; `[]` stores as
   `includes.apps: false` with an empty catalog, identical to sending `false`.
4. If `includes.activeApp` is true, the snapshot's `activeApp` is `Some(id)`,
   and `includes.apps` is a subset that does not contain `id`, return `422`:
   `the active app "scoreboard" is not among the selected apps; select it or
   leave out the Active app section`. Rationale: `merge` with `apps: all`
   replaces the catalog, so such a profile would fail activation with the
   existing `activeApp … does not name a listed app` error every time. Reject
   it at the moment the operator can fix it. (A profile with `apps: false`
   and `activeApp: true` remains allowed, as today: it is validated against
   whatever catalog exists at activation.)
5. `normalize()`, embed wallpapers, `validate()`, as today.

Request example (save three of five current apps):

```json
POST /api/v1/profiles
{
  "id": "6c1f1c1e-6f3b-4d7e-9a2e-0d2f1a9b7c11",
  "name": "Game night",
  "description": "Scoreboard and lobby only",
  "includes": {
    "displays": true,
    "backgrounds": true,
    "apps": ["scoreboard", "lobby-loop", "intermission"],
    "activeApp": true,
    "settings": false
  }
}
```

Response: `201` with the stored `SystemProfile`, whose `includes.apps` is
`true` and whose `config.apps` has exactly those three entries. The response
shape is unchanged.

#### 3.4 Activate: `POST /api/v1/profiles/{id}/activate`

Add an optional `sections` object to `ActivateProfile`:

```rust
#[derive(Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ActivateSections {
    pub displays: Option<bool>,
    pub backgrounds: Option<bool>,
    pub apps: Option<SectionSelection>,
    pub active_app: Option<bool>,
    pub settings: Option<bool>,
}

pub struct ActivateProfile {
    pub sections: ActivateSections,            // new, defaults to all None
    pub output_mappings: BTreeMap<String, String>,
    pub audio_mappings: BTreeMap<String, String>,
}
```

Rules, evaluated after the stored profile is loaded and before mappings are
applied:

- A field left out (or `null`) means "whatever the profile includes". A
  request with no `sections` at all therefore behaves exactly as today. This
  is the backwards-compatibility guarantee for existing API clients.
- `true` (or a nonempty subset) for a section the profile does not include is
  `422`: `profile "Game night" does not include displays`. The UI never sends
  this because those checkboxes are disabled, but the API must not silently
  ignore it.
- `false` for any section, included or not, means "leave this system's section
  untouched".
- `sections.apps` as a subset: every id must be in the stored apps, else
  `422`: `app "trivia" is not in this profile`. `[]` is `false`.
- Mappings are validated against the **stored** profile exactly as today,
  whether or not the corresponding section is selected. A mapping for a
  deselected section is accepted and has no effect, because the section is
  not merged. The UI omits them anyway (section 5.4).

Merge semantics. Add `SystemProfile::merge_selected(&self, current:
&DesiredState, sections: &ActivateSections) -> DesiredState` and leave the
existing `merge` as the `sections == default` case (or implement `merge` by
calling it). Per section:

| `sections.X` | Effect on the live document |
|---|---|
| omitted, profile includes X | as today: the stored section replaces the live one |
| omitted, profile excludes X | as today: untouched |
| `false` | untouched |
| `true` | the stored section replaces the live one (identical to omitted-and-included) |
| `apps: [ids]` | **upsert**: for each selected stored app, in stored order, replace the live app with the same `id` in place, or append it when there is none. Live apps that are not selected are kept. |

The upsert choice is the one genuinely new semantic (open question 1). The
reasoning: an operator who picks two of a profile's six apps is adding those
two to this system, not asking for the other four live apps to vanish; the
destructive "make the catalog exactly this" outcome stays available through
`all`, which keeps its long-documented behavior. The dialog wording in
section 5.3 states the difference in plain words next to the checkbox so the
operator is never guessing.

`activeApp` interplay: selecting `activeApp: true` with an `apps` choice that
leaves the resulting catalog without that id fails the existing
`validate_configuration_literal` check with `422` (`activeApp "x" does not
name a listed app`). No new server rule is needed; the UI pre-computes the
resulting id set (selected stored ids, plus the live ids when the choice is a
subset or none) and refuses to send a request that would fail (section 5.3).

Request example (apply displays and two apps, keep everything else):

```json
POST /api/v1/profiles/6c1f1c1e-…/activate
If-Match: "41"
If-Config-Generation: 7
If-Config-Epoch: 7d2b…
{
  "sections": {
    "displays": true,
    "backgrounds": false,
    "apps": ["scoreboard", "lobby-loop"],
    "activeApp": false,
    "settings": false
  },
  "outputMappings": { "edid:HDMI-A-1:Acme:Panel:42": "DP-1" },
  "audioMappings": { "alsa_output.pci-0000_01_00.1.hdmi-stereo": "alsa_output.usb-Vendor_Interface-00.analog-stereo" }
}
```

Response: `200` with the merged `DesiredState`, unchanged shape. Error
responses use the existing `ApiError::Validation` (`422`) and
`ApiError::Conflict` (`409`) paths; the body is the existing `{detail}` shape.

#### 3.5 OpenAPI

`CaptureIncludes`, `ActivateSections`, and `SectionSelection` derive
`ToSchema`; utoipa picks them up through the existing `request_body`
annotations, so `src/api/docs.rs` needs no new entry. Document
`SectionSelection` with a doc comment that names the three accepted forms, as
that is the text the generated API reference shows.

### 4. Save dialog (`#profile-editor`)

#### 4.1 Layout

The "Configuration sections" fieldset keeps its five checkboxes. The Apps row
becomes:

```html
<label><input type="checkbox" data-profile-include="apps" checked> Apps</label>
<span class="hint" id="profile-apps-summary" role="status">All 5 apps</span>
<button type="button" class="action ghost" id="profile-apps-choose"
        aria-describedby="profile-apps-summary">Choose apps…</button>
```

`#profile-apps-summary` reads, by state:

- all: `All 5 apps` (singular `All 1 app`); when there are no current apps:
  `No apps are configured on this system; the profile will store an empty app
  list.` (This is the existing behavior; it is now visible.)
- subset: `2 of 5 apps` followed by the ids when they fit: `2 of 5 apps:
  scoreboard, lobby-loop` (truncate after three ids with `+N more`).
- none: `Apps are not saved.`

`#profile-apps-choose` is disabled, with the summary text above as its
explanation, when `config.apps` is empty.

Below the fieldset, a `<p class="hint" id="profile-editor-note" role="status">`
shows the active-app warning from 4.5 when it applies, and is empty otherwise.

#### 4.2 The tri-state checkbox

The checkbox uses the native `indeterminate` property for `subset`. Assistive
technology reports a native indeterminate checkbox as "mixed"; do not also set
`aria-checked`, which ARIA discourages on native inputs.

State is held in a page variable, not read back from the DOM:

```js
let profileAppSelection = { kind: "all" };          // or { kind: "none" } or { kind: "subset", ids: [...] }
```

Rendering: `kind === "subset"` sets `indeterminate = true, checked = false`;
`all` sets `indeterminate = false, checked = true`; `none` sets
`indeterminate = false, checked = false`.

Click (and Space on the focused checkbox) cycles as the owner asked: from
`subset`, the next state is **`all`**; from `all`, `none`; from `none`, `all`.
`subset` is reachable only through the picker. This matches the native
behavior of an indeterminate checkbox whose `checked` is false (a click clears
`indeterminate` and sets `checked`), so the handler only has to update
`profileAppSelection` from `input.checked` on `change` and re-render the
summary; it never has to fight the browser.

#### 4.3 The picker (`#profile-apps-picker`)

One `<dialog id="profile-apps-picker" aria-labelledby="profile-apps-picker-heading">`
in the markup, shared by the Save editor and the Activate dialog (section 5),
opened through a function:

```js
async function pickApps({ title, hint, apps, selectedIds, decorate })
  // resolves to an array of ids, or null when canceled
```

Contents:

- `<h2 id="profile-apps-picker-heading">` with `title` ("Choose apps to save"
  or "Choose apps to activate").
- `<p class="hint">` with `hint` (Save: "Only the selected apps are stored in
  the profile." Activate: "Selected apps are added to this system or replace
  the app with the same id. Apps you leave unselected are not changed.").
- A `.row` with two buttons: `Select all`, `Select none`.
- A list (`<ul>` or a `<table>` in the existing `.table-scroll`), one row per
  app: `<label><input type="checkbox" data-pick-app="ID"> ID</label>` followed
  by a muted description `launcher.kind · uri` (host only, to keep rows short)
  and whatever `decorate(app)` returns as `.pill` elements (Save: the active
  app is marked `active`; Activate: `on this system` or `new`, and `not on
  this system` for a stored id the Update editor cannot preselect, see 4.4).
- A `<p class="hint" role="status">` with the live count `2 of 5 selected`.
- Footer: `Cancel` (autofocus, per the existing dialogs) and `OK`.

Behavior: `showModal()`; Escape and Cancel resolve `null` and change nothing;
OK resolves the checked ids in list order. Focus returns to the invoking
button (the existing `invoker.focus()` pattern at line 5071). Stacking a
modal on top of the modal Activate dialog is supported by the top layer; no
special handling is needed beyond returning focus.

Result mapping in the editor: `n === apps.length` → `all`; `n === 0` →
`none`; otherwise `subset`. So choosing every app in the picker lands on the
checked state, and the API receives `true`, not a list of every id.

Opening the picker preselects from the current state: `all` → every row
checked; `none` → none; `subset` → those ids. The picker never restores a
subset the operator has since clicked away from (there is no history).

The list is built from `config.apps` at the moment the picker opens. If a
`config_changed` event arrives while it is open, nothing is rebuilt; the
server's `422` for a vanished id is the backstop, and `profileTask` already
shows it in `#profile-message`.

#### 4.4 Update snapshot

`openProfileEditor(profile)` for an existing profile currently sets the five
checkboxes from `profile.includes`. It additionally fetches
`GET /profiles/{id}` (the editor already awaits `loadSystemProfiles()`; one
more request is fine) and preselects `stored ids ∩ current ids`:

- if the profile excludes apps: `none`;
- if every current app is among the stored ids: `all`;
- otherwise `subset` with the intersection, which can be empty, in which case
  the state is `none` and `#profile-editor-note` says `None of the 3 apps in
  this profile are configured on this system.`

Stored ids that are not current are listed in the picker, disabled and
unchecked, with the pill `not on this system`, so the operator sees what the
re-capture would drop. The fetch failing (for example the profile was deleted
in another browser) surfaces through `profileTask` as today and leaves the
editor closed.

#### 4.5 Validation in the editor

On every change of the Apps state or the Active app checkbox, compute: Active
app checked, `config.activeApp` set, and the selection is a subset without it.
If so, `#profile-editor-note` shows `The active app "scoreboard" is not among
the selected apps. Select it, or uncheck Active app.` and `#profile-save`
throws the same text when clicked (through `profileTask`, so it also appears
in `#profile-message`). The server returns the same `422`, so a stale page is
still safe. Save stays enabled; a disabled button with no explanation is
worse for keyboard and screen-reader users than a clear refusal.

#### 4.6 Request body

`selectedProfileIncludes()` returns `apps` as `true`, `false`, or the id array
according to `profileAppSelection`. Nothing else in the body changes.

### 5. Activate dialog

Move the dialog out of the template string into static markup
`<dialog id="profile-activate-dialog" aria-labelledby="profile-activate-heading">`
inside `#tab-profiles`, so the UI test can target stable ids and the markup is
reviewable. `activateSystemProfile(profile)` fills it. The existing guards
(dirty working copy, pending previews, precondition headers, dialog stays open
on failure, focus returns to the invoker) are kept unchanged.

#### 5.1 Structure

```
Activate “Game night”
  Sections to activate                       (fieldset)
    [x] Displays        [x] Backgrounds    [–] Apps  2 of 6 apps  [Choose apps…]
    [ ] Active app (not in this profile)   [x] System settings
    <apps hint line>
  Device mapping                             (fieldset, only when it has rows)
    <explanation>
    Saved display HDMI-A-1 · Acme Panel 42 → [ DP-1          ▾ ]  matches a connected display
    Saved display HDMI-A-2                 → [ DP-2          ▾ ]  suggested: next unassigned display
    Saved audio output alsa_output.pci-… (Living room HDMI) → [ … ▾ ]  no unassigned audio output is detected; the saved name is kept
    [Reset to suggested]
  <error line, role="alert">
  Activation replaces only the selected sections. Save or Revert any unsaved edits first.
  [Cancel] [Activate profile]
```

#### 5.2 Sections to activate

Five checkboxes `[data-profile-section="displays|backgrounds|apps|activeApp|settings"]`
with the same labels as the editor. For each section:

- profile includes it: enabled and checked on open (so a dialog the operator
  does not touch activates everything, as today);
- profile excludes it: `disabled`, unchecked, label suffixed with a muted
  `(not in this profile)`.

A fieldset hint above the row: `Choose which parts of this profile to apply.
Sections the profile does not contain are unavailable. Everything else on this
system is left as it is.`

#### 5.3 Apps, the tri-state, and the hint line

Same tri-state and picker as section 4, with its own state variable scoped to
the dialog and `pickApps({ title: "Choose apps to activate", apps:
exported.config.apps, decorate })` where `decorate` marks each stored app `on
this system` when `config.apps` has that id, else `new`. The summary span
`#profile-activate-apps-summary` reads `All 6 apps` / `2 of 6 apps` / `No
apps`.

The hint line under the row, `#profile-activate-apps-hint`, states the
server-side meaning in words:

- all: `Replaces this system's app list with the 6 apps in this profile.`
  (If the stored list is empty: `This profile stores an empty app list, so
  activating Apps removes every app from this system.`)
- subset: `Adds or updates the 2 selected apps. The other apps on this system
  are kept.`
- none: `Leaves this system's apps unchanged.`

When the profile excludes apps, the checkbox is disabled like the others and
the `Choose apps…` button is hidden.

Active-app check: when `Active app` is checked, compute the resulting id set
(all: stored ids; subset: selected ids ∪ live ids; none: live ids). If
`exported.config.activeApp` is not in it, `#profile-activate-error` shows
`The profile's active app "scoreboard" would not be in this system's app list
after activation. Select it under Apps, or uncheck Active app.` and the
Activate button throws that text when clicked. The server's `422` is the
backstop.

#### 5.4 Which mapping rows appear

- Display rows: one per distinct saved `OutputMatch` key in
  `exported.config.outputs` (as `profileNames(exported, "display")` builds
  them today), shown only when the Displays section is checked. Unchecking
  Displays hides the rows (`hidden`, not removed, so typed values survive a
  toggle) and omits their mappings from the request.
- Audio rows: one per distinct `audio.output` string among the stored apps
  that the Apps choice will apply (all: every stored app; subset: the
  selected ones; none: no rows). Recomputed when the Apps choice changes;
  values the operator typed for a row that stays are kept.
- No rows at all: the Device mapping fieldset is replaced by `<p
  class="hint">This profile has no display or audio device names to map.</p>`.

#### 5.5 The combo box

Native combo: `<input id="profile-map-N" list="profile-map-displays">` or
`list="profile-map-audio"`, with two `<datalist>` elements rebuilt on open
from the detected devices:

- `#profile-map-displays`: one `<option value="HDMI-A-1">Acme Panel 42</option>`
  per entry of `outputs`, in daemon order; the label is `make model serial`
  joined by spaces when any is present.
- `#profile-map-audio`: one `<option value="alsa_output.…">Living room
  HDMI</option>` per entry of `sinks` with `isNullSink == false`, in daemon
  order; the label is `description` when present, plus ` (default)` for
  `isDefault`.

A datalist input is a real combo: it accepts free text, offers the list on
focus or arrow-down, filters as the operator types, and is labeled by its
`<label for>`. It needs no ARIA of its own. (A custom `role="combobox"`
widget is open question 3; the native control is recommended because this is
a single-file UI with no component library.)

The row label is a `<label for="profile-map-N">` whose text is `Saved display
HDMI-A-1 · Acme Panel 42` (connector, then the EDID parts that the rule
specifies) or `Saved audio output alsa_output.pci-… (Living room HDMI)` (the
saved name, then the description if a detected sink has that id). The input's
`placeholder` is `Keep HDMI-A-1` / `Keep alsa_output.pci-…`, so a blank field
says what it does. A `<span class="hint" data-profile-map-status>` after the
input is linked by `aria-describedby` and carries the status text from 5.6.

#### 5.6 Default assignment when the dialog opens

Run independently for displays and for audio, each over its rows in display
order and its detected devices in index order. A device is "assigned" once
any row in this dialog holds it, in either pass.

Pass 1, exact matches. For each row in order:

- Display row: the saved rule selects a detected output when every field the
  rule specifies equals the output's field (`name`, `make`, `model`,
  `serial`), the same rule as `OutputMatch::matches` in
  `src/model/desired.rs`. Comparison is exact and case-sensitive: connector
  names and EDID strings are identifiers, and `HDMI-A-1` and `hdmi-a-1` are
  not the same connector. Assign the lowest-index unassigned output the rule
  selects (two identical panels matched by make and model alone take the
  first unused one). Status: `matches a connected display`.
- Audio row: a detected sink whose `id` equals the saved name exactly.
  Status: `matches a detected audio output`.

Pass 2, fill. For each row still blank, in order: assign the lowest-index
detected device not yet assigned in this dialog. Status: `suggested: the next
unassigned display` / `suggested: the next unassigned audio output`. When no
unassigned device remains, leave the field blank; status: `no unassigned
display is connected; the saved name is kept` / `no unassigned audio output
is detected; the saved name is kept`.

The `Reset to suggested` button reruns both passes against the current
`outputs` and `sinks` and overwrites every field. Nothing reruns
automatically when an SSE update changes `outputs` or `sinks` while the
dialog is open; the datalists are rebuilt on the next open.

This replaces today's comparison against the *configured* output keys and
sink names (`profileNames(config, …)`), which could suggest a display that is
configured but not plugged in and never suggested anything for a name the
system had not seen.

#### 5.7 What is sent

For each visible row with a nonblank trimmed value:

- Display row: send `outputMappings[row.key] = value` **unless the saved rule
  already selects a detected output with that connector name** (the Pass 1
  case). Sending nothing there preserves an EDID-qualified rule and its
  `adopted` values; sending the mapping would rewrite it to
  `OutputMatch::by_name` and clear `adopted` (`src/api/profiles.rs` line
  189), which is only wanted when the operator is pointing the rule at a
  *different* display. Today's code sends nothing only when the value equals
  the key string, which for an EDID rule it never does.
- Audio row: send `audioMappings[saved] = value` unless `value === saved`.

A blank field sends nothing: the saved name is kept verbatim, and if that
device is absent the activation still succeeds and the usual status
divergences report it (existing documented behavior).

Two display rows holding the same connector name show
`#profile-activate-error`: `DP-1 is chosen for two saved displays. Each
display on this system can take the place of one saved display.` and the
Activate button refuses with the same text; the server would reject the
merged document anyway (`duplicate output key`). Two audio rows on one sink
are fine and get no warning.

#### 5.8 Explanation text

Fieldset hint for Device mapping, replacing the current sentence:

> The profile refers to displays and audio outputs by the names they had on
> the system where it was saved. For each saved name, choose the device on
> this system that should take its place: pick a detected device from the
> list or type a name. Leave a field blank to keep the saved name.

### 6. Accessibility, wording, states

- en-US throughout (`scripts/check-en-us.sh` runs in CI and catches the
  usual double-l and -ise forms). Sentence case for buttons, as in the rest
  of the page.
- Tri-state via the `indeterminate` property only (section 4.2).
- Every input has a `<label for>`; every status or warning paragraph that
  changes while the dialog is open has `role="status"` (summaries, counts,
  mapping statuses) or `role="alert"` (`#profile-activate-error`,
  `#profile-editor-note` when it carries a refusal).
- Disabled section checkboxes carry the visible `(not in this profile)` text
  in their label, so the reason is read with the control.
- Keyboard: Tab order follows the visual order; Space toggles checkboxes;
  Escape cancels any dialog without side effects; focus returns to the
  invoking button on close (existing pattern). The picker's `OK` is the
  default action when Enter is pressed inside it; the Activate dialog keeps
  `Cancel` as the autofocused control, as the other dialogs do.
- Empty states: no current apps (4.1); profile with no apps (5.3 hint and no
  audio rows); no detected displays or sinks (the datalist is empty, the
  combo still accepts typing, and every row's status says the saved name is
  kept); no mapping rows at all (5.4).
- Error states: server `422`/`409` texts are shown verbatim in
  `#profile-activate-error` and the dialog stays open (existing behavior,
  tested); the editor shows them in `#profile-message` (existing).
- Width: the Activate dialog gets `max-width: min(44em, calc(100vw - 32px))`
  (the base `dialog` rule at line 447 is `34em`), since each mapping row now
  carries a label, a combo and a status.

### 7. Tests

#### 7.1 Rust, `src/api/profile_tests.rs`

Each test is a sentence, per `CONTRIBUTING.md`.

1. `capture_with_an_app_subset_stores_only_those_apps_in_catalog_order` —
   live apps `[a, b, c]`, request `includes.apps: ["c", "a"]`; export has
   `config.apps` ids `[a, c]` and `includes.apps == true`.
2. `capture_rejects_an_app_id_that_is_not_in_the_current_configuration` —
   `["a", "zzz"]` → `422`, no profile stored (list is empty).
3. `capture_with_an_empty_app_list_excludes_the_apps_section` — `[]` → export
   `includes.apps == false`, `config.apps == []`.
4. `capture_rejects_an_active_app_outside_the_selected_subset` —
   `activeApp: "a"`, `includes.apps: ["b"]`, `activeApp: true` → `422`; the
   same with `includes.activeApp: false` → `201`.
5. `capture_still_accepts_a_boolean_apps_flag` — `includes.apps: false` and
   `true` behave as before (regression guard for the untagged enum).
6. `activation_without_sections_behaves_as_before` — covered by the existing
   tests, which must keep passing unchanged.
7. `activation_with_an_app_subset_upserts_and_keeps_other_live_apps` — live
   `[keep, shared(old uri)]`, stored `[shared(new uri), extra, unused]`,
   `sections.apps: ["shared", "extra"]` → live `[keep, shared(new uri),
   extra]` in that order.
8. `activation_with_apps_false_leaves_the_live_catalog_untouched` and
   `activation_with_apps_true_replaces_the_live_catalog`.
9. `activation_rejects_a_section_the_profile_does_not_include` —
   `sections.displays: true` on a profile with `includes.displays: false` →
   `422`, live config unchanged.
10. `activation_rejects_an_app_id_the_profile_does_not_store` → `422`.
11. `activation_with_sections_false_leaves_those_sections_untouched` — a
    profile including everything, `sections: {settings: false, backgrounds:
    false}` → settings and backgrounds unchanged, outputs and apps replaced.
12. `activation_with_an_app_subset_that_drops_the_active_app_is_invalid` —
    `sections.apps: ["b"]`, `activeApp: true`, stored active `a`, live
    catalog without `a` → `422`.
13. `activation_accepts_a_mapping_for_a_deselected_section` — `outputMappings`
    present with `sections.displays: false` → `200`, outputs unchanged.

Unit tests in `src/profiles.rs`:

- `merge_selected_upsert_replaces_in_place_and_appends_in_stored_order`.
- `section_selection_deserializes_true_false_and_lists` (including `[]` is
  none, duplicates collapse).

#### 7.2 Browser, `tests/ui/system-profiles.cjs`

Fixture changes: serve `GET /outputs` with the four fixture outputs
(`HDMI-A-1` … `HDMI-A-4`, with `make: "Acme", model: "Panel", serial: "42"`
on the first) instead of the catch-all `[]`; serve `GET /av` with two real
sinks and one `isNullSink`; give the exported profile three stored apps
(`stage` with `audio.output: "profile-sink"`, `lobby`, `unused`) and an
`activeApp` of `stage`. The fixture's current config has `renderer-1` …
`renderer-4` and `activeApp: renderer-1`.

Save editor:

- Open `#profile-new`, assert the summary reads `All 4 apps`; open
  `#profile-apps-choose`, uncheck `renderer-3` and `renderer-4`, OK; assert
  `[data-profile-include="apps"]` has `indeterminate === true` and `checked
  === false`, the summary says `2 of 4 apps`, and the POST body has
  `includes.apps` deep-equal `["renderer-1", "renderer-2"]`.
- Click the indeterminate checkbox: `indeterminate === false`, `checked ===
  true`, summary `All 4 apps`; click again: unchecked, summary `Apps are not
  saved.`; a save in each state sends `true` and `false`.
- Uncheck `renderer-1` (the active app) in the picker with Active app checked:
  `#profile-editor-note` matches `/active app .* not among/` and Save sends
  nothing; uncheck Active app and Save succeeds.
- Picker Escape leaves the state unchanged.
- With a fixture config that has no apps, `#profile-apps-choose` is disabled
  and the summary matches `/No apps are configured/`.

Activate dialog:

- For a profile with `includes.activeApp: false`, `[data-profile-section=
  "activeApp"]` is disabled and its label contains `not in this profile`.
- Choose apps `stage` only: the request has `sections.apps` deep-equal
  `["stage"]`, the hint matches `/Adds or updates the 1 selected app/`, and
  exactly one audio row exists (for `profile-sink`); choose none: no audio
  row, `sections.apps === false`.
- Default assignment: row 0 (`edid:HDMI-A-1:Acme:Panel:42`) shows `HDMI-A-1`
  with status `/matches a connected display/`; row 1 (`NOT-CONNECTED`) shows
  `HDMI-A-2` with status `/suggested/` (replacing the current assertion that
  it is blank). The audio row shows the first non-null sink.
- `input#profile-map-0` has `list="profile-map-displays"` and the datalist
  has four options in fixture order; the audio datalist has two.
- Row 0 left at its exact match sends no `outputMappings` entry for its key;
  typing `HDMI-A-3` into row 0 sends `outputMappings["edid:HDMI-A-1:Acme:
  Panel:42"] === "HDMI-A-3"`; clearing row 1 sends nothing for
  `NOT-CONNECTED`.
- Typing `HDMI-A-3` into two display rows shows `#profile-activate-error`
  matching `/chosen for two saved displays/` and sends no request.
- `#profile-map-reset` restores the defaults after edits.
- Unchecking Displays hides the display rows and the request has no
  `outputMappings` keys.
- Existing assertions (precondition headers, stale `409` keeps the dialog
  open, unsaved-edit guard) stay.

Update `tests/ui/README.md`'s paragraph on the System Profiles check to list
the new behaviors.

### 8. Documentation

#### 8.1 `docs/configuration.md`, "System Profiles"

- `includes.apps` on create and update: `true`, `false`, or an array of app
  ids from the current configuration; the stored profile always records a
  boolean; the active-app rule.
- `sections` on activate, with the table from 3.4 and a short example; the
  upsert wording; omitted means the profile's `includes`; `422` cases.
- Mapping paragraph: say what a mapping is for in the words of 5.8, that a
  key is the saved output match key or saved sink name, and that a mapping
  for a deselected section is ignored.

#### 8.2 New page `docs/system-profiles.md` ("System profiles" in `mkdocs.yml`
nav, after Configuration)

User-facing, task-oriented, linking to the reference above. It should explain:

1. What a profile is and is not: a named snapshot of chosen sections; never
   bootstrap settings (token, bind address, state directory); saving never
   changes the live system, activating does.
2. The five sections and what each carries, with one line each, including
   that Apps and Active app are independent.
3. Saving: name rules (unique, trimmed, case-insensitive), the sections
   fieldset, the tri-state Apps box with the picker, what the summary text
   means, why Save refuses when the active app is left out.
4. Update snapshot: it re-captures from the current configuration; which
   stored apps are preselected; what happens to stored apps that no longer
   exist.
5. Activating: the Save-or-Revert precondition and why; the Sections fieldset
   and the exact meaning of all, selected and none for Apps (same three
   sentences as the dialog); that disabled sections are absent from the
   profile.
6. Device mapping: the problem it solves (a profile saved on one system names
   that system's connectors and sinks), how defaults are chosen (exact match
   first, then the next unassigned detected device, in the order the daemon
   lists them), what blank means, when to type a name that is not detected
   (a display you will plug in later), and that an absent device is reported
   as a divergence rather than blocking activation.
7. Export, import and duplicate: portable JSON with embedded wallpapers, the
   name prompt, and that import never activates.
8. Screenshots: the Profiles tab with saved profiles; the Save editor with
   the app picker open; the Activate dialog with sections and mapping rows.
   Add the three shots to `SHOTS.healthy` in `scripts/screenshots/capture.cjs`
   (`profiles`, `details/profile-save-apps`, `details/profile-activate`). The
   capture must create a temporary profile through the API, take the shots,
   and delete it in `finally`, because profiles are not part of the working
   copy that the script already restores. The redaction pass in the capture
   script covers real machine and project names.
   Register the images in `docs/screenshots.md` under a new "System
   profiles" heading with the usual `Read more` line.

Run `scripts/check-machine-names.sh` and `scripts/check-en-us.sh` on the new
page; any test-system alias it mentions must link to the test systems page
as the checker requires.

### 9. Open questions

Each with the default this spec assumes.

1. **Subset activation: upsert or replace?** Default: upsert (3.4). The
   alternative, "the catalog becomes exactly the selected apps", makes
   picking fewer apps delete more, which is the opposite of what a picker
   suggests, and is already reachable by trimming the profile at save time.
2. **Click from subset goes to all or to none?** Default: all (4.2). It is
   the browser's native result for an indeterminate box with `checked ===
   false`, and "all" is the non-destructive direction. `none` would need the
   handler to override the native toggle.
3. **Native `<input list>` + `<datalist>` or a custom ARIA combobox?**
   Default: native. Chromium and Firefox both show the list on focus and
   filter on typing, and it costs no code. Styling of the drop-down is the
   browser's; if the owner wants the dark theme inside the list, a custom
   widget is the price.
4. **Should the display list include connectors with nothing attached
   (`GET /api/v1/ports`)?** Default: no; the list is of detected displays,
   which is what the request asked for. A connector that is empty today can
   still be typed. Revisit if operators configure sockets ahead of plugging in.
5. **Active app outside the saved subset: refuse or drop `activeApp`?**
   Default: refuse with `422` at capture (3.3 step 4), so the operator decides
   rather than discovering a silently emptied Active app later.
6. **Add `appIds` (or an app count) to `ProfileSummary` so the table can show
   "Apps (3)" without a per-row GET?** Default: not now. The Update editor
   fetches the one profile it edits, which is one request, and the table
   column stays as it is.
7. **Case sensitivity of the exact match.** Default: case-sensitive and
   whitespace-exact (5.6). Connector names and PipeWire node names are
   identifiers; a case-insensitive match could pick the wrong one of two
   sinks that differ only in case, which PipeWire allows.
8. **Prefer PipeWire's default sink (`isDefault`) for the first unassigned
   audio row instead of index 0?** Default: index 0, as the request specifies.
   Noted because on a machine whose first-listed sink is an unused HDMI
   output, the default sink is the better guess; cheap to change later.
9. **Hide or disable mapping rows of a deselected section?** Default: hide
   (`hidden` attribute, values retained). Disabled rows keep the dialog long
   for no benefit.
10. **Preselect the stored subset in Update snapshot?** Default: yes (4.4). It
    costs one GET and makes "update this profile" keep its shape instead of
    silently widening to every current app.

### 10. Implementation slices

Slices 1 and 2 are backend and independent of each other once the shared
`SectionSelection` type from slice 1 exists (slice 2 may land first with its
own copy of the type if scheduled that way; reconcile on merge). Slices 3, 4
and 5 are UI and build on each other in that order. Slice 6 is tests for 3–5
and can be written in parallel against this spec's ids. Slice 7 is docs and
needs 3–5 for the screenshots only. Do not run builds in parallel per-agent
target directories (see the build disk budget note in memory).

#### Slice 1 — Capture an app subset

- Files: `src/profiles.rs` (add `SectionSelection`), `src/api/profiles.rs`
  (`CaptureIncludes`, `capture()` filtering and the two `422` rules),
  `src/api/profile_tests.rs` (tests 1–5 of 7.1), the unit test for
  `SectionSelection`.
- Acceptance: tests 1–5 pass; every existing test in `profile_tests.rs` and
  `profiles.rs` passes unchanged; `GET /profiles/{id}` of a subset profile
  has `includes.apps == true`; `cargo clippy` clean; the OpenAPI document
  (`cargo run -- --openapi` or the build-docs path) shows `includes.apps` as
  `boolean | string[]`.
- Suggested model: sonnet. Mechanical serde and filtering with a precise
  spec; moderate blast radius, fully covered by tests.

#### Slice 2 — Selective activation with app upsert

- Files: `src/profiles.rs` (`ActivateSections` or its model-side twin,
  `merge_selected`, unit test), `src/api/profiles.rs` (`ActivateProfile.
  sections`, validation, handler uses `merge_selected` in both the
  pre-validation merge and the `commit_literal_if` closure),
  `src/api/profile_tests.rs` (tests 7–13), `docs/configuration.md` API text
  for `sections` (8.1).
- Acceptance: tests 7–13 pass; existing activation tests pass unchanged (the
  no-`sections` path is byte-for-byte the old behavior); a `sections` value
  for an excluded section is `422` and leaves the live document and wallpaper
  files untouched; `cargo clippy` clean.
- Suggested model: opus. The merge closure runs under the state lock and the
  two merge call sites must agree; ordering and upsert semantics are the one
  place a subtle mistake corrupts an operator's catalog.

#### Slice 3 — Save editor: tri-state Apps and the shared picker

- Files: `src/api/ui/index.html` (markup in `#profile-editor`, the
  `#profile-apps-picker` dialog, `pickApps()`, `profileAppSelection`,
  `selectedProfileIncludes()`, `openProfileEditor()` incl. the Update
  preselection, the editor note).
- Acceptance: the Save-editor assertions in 7.2 pass; no `pageerror` in the
  test run; the indeterminate state is set via the property and no
  `aria-checked` is added to native inputs; the picker returns focus to the
  button that opened it; the summary and note texts match this spec
  verbatim; en-US check passes.
- Suggested model: sonnet. Contained DOM work with exact strings and ids
  given; the test slice verifies it.

#### Slice 4 — Activate dialog: static markup and sections

- Files: `src/api/ui/index.html` (`#profile-activate-dialog` markup,
  `activateSystemProfile()` rewrite to fill it, section checkboxes, apps
  tri-state reusing `pickApps()`, hint line, active-app check, `sections` in
  the request body, row visibility by section).
- Acceptance: the section and apps assertions in 7.2 pass; the existing
  guard, header and `409` assertions still pass; a profile including all five
  sections and left untouched sends `sections` with all `true`, which the
  server treats identically to the old request.
- Suggested model: sonnet.

#### Slice 5 — Device mapping combos and default assignment

- Files: `src/api/ui/index.html` (datalists, row template, the two-pass
  assignment function — pure, taking `rows`, `outputs`, `sinks` and returning
  assignments so it is unit-testable in the browser test — statuses, reset
  button, duplicate-display check, the send rule in 5.7, the explanation
  text, dialog width).
- Acceptance: the default-assignment, datalist, send-rule, duplicate and
  reset assertions in 7.2 pass; an EDID-qualified saved rule whose display
  is connected produces no `outputMappings` entry; a saved name with no
  detected device and no spare device sends nothing and shows the "saved name
  is kept" status.
- Suggested model: opus. The assignment algorithm and the send rule have
  several interacting edge cases (EDID rules, duplicates, running out of
  devices) and the owner's stated frustration is clarity, which needs
  judgment about wording and states.

#### Slice 6 — Browser test update

- Files: `tests/ui/system-profiles.cjs`, `tests/ui/README.md`.
- Acceptance: every assertion listed in 7.2 is present and the file passes
  against slices 3–5; the test still intercepts every request and never
  contacts an appliance; the reversed assertion about the unknown display is
  accompanied by a comment saying why.
- Suggested model: sonnet (tester agent). The spec lists each assertion;
  the work is translating them to Playwright against known ids.

#### Slice 7 — User docs page and screenshots

- Files: `docs/system-profiles.md` (new), `mkdocs.yml` (nav), `docs/
  screenshots.md`, `docs/configuration.md` (cross-links and the capture
  `includes.apps` text if slice 2 did not already cover it), `scripts/
  screenshots/capture.cjs` (three shots with temporary-profile setup and
  teardown), `docs/assets/screenshots/profiles.avif`, `docs/assets/
  screenshots/details/profile-save-apps.avif`, `docs/assets/screenshots/
  details/profile-activate.avif`.
- Acceptance: `mkdocs build --strict` passes (`scripts/dev-check.sh docs`);
  `scripts/check-machine-names.sh` and `scripts/check-en-us.sh` pass; the
  page covers every item in 8.2; the capture script leaves the appliance
  with no leftover profile and an unchanged working copy; images carry
  `width`/`height` attributes as the script writes them; no private project
  name or real machine name appears in any screenshot (review by eye, as the
  README asks).
- Suggested model: sonnet for the page text and nav; the screenshot capture
  needs an appliance and is run by a person or the main session.
