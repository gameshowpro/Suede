# Multiple clients

A Suede appliance is usually driven by more than one client at a time: the
web page on an operator's laptop, a second page on a tablet at the wall, a
show controller writing over the API, a script. This page explains what
happens when they change the configuration at the same time, and what a
client has to do to behave well.

## One document, one working copy

The appliance holds one configuration document. Every change goes through the
API, is validated as a whole, and is applied to the outputs at once. There is
no per-client session and no locking.

On top of the saved document the appliance holds at most **one uncommitted
change set**, the *working copy*. Its only purpose is to let someone defer
the decision to save or revert: drag a corner pin, look at the wall, then
decide. It does not belong to whoever created it. Any client may change it,
save it or throw it away, and every other client sees that happen.

| Action | Request | Effect |
| --- | --- | --- |
| Preview | `PUT /api/v1/config` with `"committed": false` | Replaces the working copy; the outputs follow immediately; nothing is written to disk. |
| Save | `PUT /api/v1/config` with `"committed": true`, or any section `PUT` | Replaces the saved document, writes it to disk, and clears the working copy. A section write while a working copy exists saves that working copy together with the new section. |
| Revert | `POST /api/v1/config/revert` | Discards the working copy; the outputs return to the saved document. |

`GET /api/v1/config` and the section reads return the **effective** document:
the working copy if one exists, otherwise the saved one. Its `committed` field
says which. The same flag appears in `GET /api/v1/status`, so a client that only
watches status still knows whether the wall is showing something unsaved.

## Every change is announced

Every change to the effective document is published on the event stream
(`GET /api/v1/events`) as a `config_changed` event. That includes previews,
saves, reverts, values the appliance adopts on its own (a display's settled
mode, for example) and repairs made while loading an old state file. The
event carries the whole document, so a client never has to fetch it again:

```json
{
  "revision": 41,
  "generation": 118,
  "epoch": "7b0c…-3",
  "committed": false,
  "section": "all",
  "config": { "schemaVersion": 2, "revision": 41, "committed": false, "outputs": [ "…" ] }
}
```

A client should treat this event as the truth and replace what it holds. The
web page does exactly that: on every event it applies the received document,
re-renders, and sets its Save and Revert buttons from `committed`. The only
thing it keeps for itself is the text in the control that has keyboard focus,
so a keystroke is not lost because someone else moved a pin at the same
moment. When that control loses focus, the value the operator typed is what
gets sent.

Events are state-based, not a replayable log. A client that reconnects
should fetch the document once and then apply events.

## What two people editing at once looks like

Suppose A is adjusting a corner pin on the laptop while B changes the black
lift on the tablet.

1. A's page sends a preview after each drag. The working copy now carries A's
   pin. B's page receives the event and shows A's pin moving.
2. B changes the black lift. B's page sends a preview built from the document
   it holds, which already includes A's pin, so the working copy now carries
   both changes. A's page receives the event and shows B's lift.
3. Either of them presses Save. The whole working copy, both changes, is
   saved. The other page's Save button goes quiet because the event says
   `committed: true`.
4. Had one of them pressed Revert instead, both changes would be gone, on both
   pages, at the same moment.

Nothing prevents B from saving or reverting A's work. That is intended: the
working copy is a property of the appliance, and the wall can only show one
thing. The protection against surprise is visibility, not ownership.

## Lost updates, and when to use preconditions

Because every write carries the whole document, or a whole section, two
writers can still race: if B builds a document from a stale copy and sends
it after A's change, A's change is overwritten. A client that applies
`config_changed` as it arrives rarely holds a stale copy for more than a
network round trip, so for interactive use this is not a practical problem.

A script or controller that must not overwrite anything it has not seen can
say so. Every configuration response carries three headers, and a write may
echo them back as conditions:

| Response header | Request header | Meaning |
| --- | --- | --- |
| `ETag: "41"` | `If-Match: "41"` | The saved document's revision. |
| `X-Config-Generation: 118` | `If-Config-Generation: 118` | The identity of the effective document, working copy included. |
| `X-Config-Epoch: 7b0c…-3` | `If-Config-Epoch: 7b0c…-3` | The daemon's run; a restart starts a new one. |

A write that names a value which no longer matches is refused with `409` and
the current values, and the client can fetch, merge and retry. Preconditions
are optional. A write without them is applied to whatever is current, which
is what an interactive client wants.

```sh
gen=$(curl -sD - -o /dev/null http://appliance:9088/api/v1/config | awk 'tolower($1)=="x-config-generation:"{print $2}' | tr -d '\r')
curl -X PUT http://appliance:9088/api/v1/config/settings \
  -H 'content-type: application/json' -H "If-Config-Generation: $gen" \
  -d '{"hideCursor": true}'
```

## High-rate writers {: #high-rate-writers }

A slider or a show controller driving a value at interactive rates —
dragging an overlap slider, nudging black lift — is a different traffic
pattern from an operator's occasional edit, and needs different discipline
from the client to stay smooth for everyone watching.

- **Keep one request in flight and one pending.** Do not queue every
  intermediate value: while a request is outstanding, hold at most one more,
  and replace it with the newest value whenever the slider moves again. When
  the in-flight request completes, send the pending one, if any, and drop
  whatever value it superseded. A client that instead queues every
  `mousemove` falls further and further behind the pointer.
- **Send `committed: false` while dragging.** Each preview replaces the
  working copy, same as any other preview, and the outputs follow
  immediately. On release, either send one final write with
  `committed: true` to persist it, or leave it uncommitted and let the
  operator use Save — both are legitimate, and the choice belongs to the
  client's own UI, not to the protocol.
- **Do not use `?wait`.** It blocks the response until a reconciliation pass
  has run, which is the opposite of what a client sending dozens of writes a
  second wants.
- **Do not send preconditions.** `If-Match`, `If-Config-Generation`, and
  `If-Config-Epoch` exist to refuse a write built on a stale copy; a slider
  has no stale copy to protect, since each of its writes is meant to
  overwrite the one before it, from itself or from anyone else. Sending
  preconditions from a slider only invites spurious `409`s.
- **Expect the event stream to thin a burst.** `config_changed` carries the
  whole document, so a slider at 60 Hz is 60 full documents a second to every
  subscriber. The server coalesces: after each event it drains whatever is
  already queued and forwards only the newest `config_changed` of that
  drained run, so a watcher may not see every intermediate value — only the
  latest one at the rate it can keep up. It always receives the final value;
  it just cannot assume it saw every step in between. Apply what arrives at
  most once per animation frame if you are re-rendering a whole panel from
  it, exactly as the reference UI does.
- **Accept that another client may Save or Revert mid-drag.** The working
  copy still belongs to nobody. If another operator presses Revert while your
  slider is dragging, the next `config_changed` says so, and your slider's
  next write simply previews on top of whatever is there now — there is no
  special case for "someone moved my working copy out from under me",
  because that is the model everywhere, not just here.

[Grid arrangement](configuration.md#grid-arrangement) is the worked example:
`PUT /api/v1/config/projection/arrangement` with `committed: false` on every
slider tick, a final `committed: true` (or a plain Save) on release, and no
`?wait` or preconditions in between. Its CPU-renderer caveat matters here
too: a CPU-rendered appliance restarts its slicer on every one of those
ticks, so interactive dragging is a GPU-renderer feature in practice.

## Validation is whole-document

A preview or a save is validated as the complete document it produces, not
as a delta. If A's pin and B's lift are individually fine but together make
the document invalid, the second write is refused with `422` and the working
copy is left as it was. The web page rolls its own controls back to the last
document the appliance confirmed and shows the reason.

## What survives a restart

Only the saved document is written to disk. A working copy is lost when the
daemon restarts, and clients learn of the restart through a new
`X-Config-Epoch`. If a save is accepted but the disk write fails, the change
stays live and every client sees it, and `GET /api/v1/status` reports
`state_not_persisted` until a later save succeeds; see
[Troubleshooting](troubleshooting.md#state-not-persisted).

## Rules for a well-behaved client

- Subscribe to `GET /api/v1/events` and apply every `config_changed` as the
  current truth. Fetch the document once on connect and after a reconnect.
- Build every write from the newest document you hold, and send the whole
  document or a whole section.
- Show the `committed` flag. An operator should always know whether the wall
  is showing something unsaved, and that someone else may save or revert it.
- Use `committed: false` for anything a person is still deciding about, and
  `committed: true` only when they have decided. Do not leave a working copy
  behind when the person walks away.
- Send preconditions only when overwriting an unseen change would be worse
  than a refused write. Handle `409` by fetching, merging and retrying.
- Do not assume you own the working copy. If an event says it is gone, it is
  gone.

The full request and response shapes are in the
[API reference](api/index.html) and the configuration rules in
[Configuration](configuration.md#live-preview).
