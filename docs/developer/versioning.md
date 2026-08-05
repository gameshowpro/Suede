# Versioning

Two numbers, decided in two different ways. The release version is a
judgement and lives in `Cargo.toml`; everything else about a build is derived
from git and never typed by anyone.

## Before you commit

**Nothing.** Ordinary commits do not touch the version. Push as often as you
like; CI builds every push and publishes nothing, because the tag it would
create already exists.

**Bump `Cargo.toml` only when you intend to release**, which is the one
decision a person has to make.

Pushing that bump to `main` is what makes a release: CI tags `v{version}`,
builds both architectures, and attaches the packages. Pushing anything else
builds and tests but publishes nothing.

## Which number to move

Features go in the second, fixes in the third, and packaging-only changes
never touch the version at all — they land in the Debian revision, which is
derived rather than typed. One adjustment applies while the major is still
`0`, which is [today](#while-the-major-is-0).

Ask the questions **in this order**. The first one that answers yes decides
it, because compatibility outranks how the change feels:

1. **Will a saved configuration stop loading, or start meaning something
   different?** → breaking (see the note below for which digit that is today)
2. **Can an operator do something they could not do before?** → minor
3. **Otherwise** → patch

The order matters because the two ways of asking can disagree. A change you
would naturally call a fix — tightening a field that was wrongly permissive —
still refuses somebody's saved document, and the version has to say so. When
"feature or fix?" and "does it still load?" point at different digits, the
compatibility answer wins.

| Change | Move |
|---|---|
| A field is removed or renamed; a document that used to load is refused; an existing field changes meaning | breaking |
| A default changes such that an unattended box behaves differently after upgrade | breaking |
| A new field, endpoint, preset, or capability | minor |
| An existing field accepts something it used to reject | minor |
| A fix that changes no shape: wrong output, a crash, a bad blend ramp | patch |
| Docs, tests, CI, refactoring with no outward effect | nothing |
| Packaging: unit file, dependencies, install scripts, cargo-deb settings | nothing — see [the revision](#what-a-package-calls-itself) |

The second row catches the case that is easiest to under-call. Moving the
default API port from 9080 to 9088 changed no schema and refused no document,
so it looks like a patch — but an appliance that nobody logs into comes back
after the upgrade on a port its operator is not expecting, which is
indistinguishable from the box being dead. Behaviour an unattended machine
depends on is part of the contract, the same as a field name.

Suede is an appliance daemon, not a library, so nobody resolves against its
version as a dependency. What the number is *for* is telling an operator
whether an upgrade can break the box they already configured — which is why
the table is about documents and behaviour, not about how much code moved.

### While the major is 0

!!! warning "The breaking slot is the minor, not the major"
    Suede is `0.1.0`. By semver convention nothing below `1.0.0` promises
    stability, so a breaking change goes `0.1.x` → `0.2.0`, and patch carries
    everything else. Read "breaking" in the table above as **minor** for now,
    and "minor" as patch.

    At `1.0.0` the three columns line up with their names and the table reads
    literally. That is the point to reach deliberately — it is a promise that
    saved configurations survive every `1.x` upgrade, not a milestone to drift
    into.

### The fourth number, and where it went

In a four-part .NET version the last field is for build and packaging
revisions that do not change the contents. Debian has that field too, and it
is the revision after the hyphen — `0.1.0-1` — which is precisely why the
commit height lives there and not in the patch digit.

The difference is that you never type it. Fixing a unit file or a dependency
is a commit, so the height advances and the revision moves on its own:

```
0.1.0-1+13.g8096265   →   0.1.0-1+14.gcb0377e
```

Same source version, next packaging build, and it upgrades cleanly. Nothing
to remember.

!!! note "Repackaging a release with no code change"
    A build sitting exactly on its tag takes a plain `-1`, so a packaging fix
    to an already-published release is the one case the derivation does not
    cover: you would produce `0.1.0-1` a second time with different contents.
    Tag it again (`v0.1.1`) if anything reached a user, or pass
    `--deb-revision 2` by hand if it did not. Re-issuing the same version with
    different contents is the thing to avoid, whichever route you take.

### Breaking changes move two numbers

The package version is for a person; `SCHEMA_VERSION` in
[`src/model/desired.rs`](https://github.com/gameshowpro/Suede/blob/main/src/model/desired.rs)
is the machine's version of the same statement. A document stamped with a
schema newer than the daemon understands is refused outright rather than
half-read.

So a breaking configuration change bumps **both**, in the same commit. Moving
the release version alone leaves the daemon quietly accepting a document it
should reject; moving `SCHEMA_VERSION` alone means an operator gets a refusal
from a version number that promised compatibility. Either way round, the two
answers disagree, and that is a bug rather than an oversight.

## What a build calls itself

`git describe` is the source, so every build can be traced to a commit:

```
$ suede --version
suede 0.1.0 (v0.1.0-12-g81226ee)
```

The release it is meant to be, then the last tag, how far past it, and the
exact commit. A working tree with uncommitted changes says so — `-dirty` —
which matters most on the machine where the changes are being made.

`GET /api/v1/system` reports the same thing as `buildId` beside
`suedeVersion`, so it can be read from a machine you are not sitting at.

This exists because `0.1.0` alone cannot answer the question anyone actually
has. Installing a fix and seeing no change, you cannot tell whether the
package failed to install, the daemon failed to restart, or the fix was never
in that build. That happened, and it cost an afternoon.

!!! note "Building without git"
    A source tarball has no history, so the build script falls back to
    `unknown` and `--version` prints the bare version. Set `SUEDE_BUILD_ID`
    to state the answer instead — CI does exactly this, because `cross`
    builds inside a container that has neither the history nor git itself.

## What a package calls itself

The Debian revision carries the same information, in a form dpkg can order:

| Where HEAD is | Version | Meaning |
|---|---|---|
| Exactly on the tag | `0.1.0-1` | The release |
| 12 commits past it | `0.1.0-1+12.g81226ee` | A development build |

`+` sorts above a plain `1`, so a development build supersedes the release it
follows, and `0.2.0-1` still supersedes both. The **height** is what makes
successive development builds ordered.

The suffix does order, and that is not an accident of formatting. dpkg
compares a version by splitting it into alternating runs of non-digits and
digits, comparing the digit runs **numerically**. So `+9.` and `+12.` are
compared as 9 against 12, not as the strings `9` and `12`:

```
0.1.0-1+9.gaaaaaaa   <  0.1.0-1+12.gbbbbbbb
0.1.0-1+12.gbbbbbbb  <  0.1.0-1+100.gccccccc
0.1.0-1              <  0.1.0-1+1.gaaaaaaa     the release, then builds after it
0.1.0-1+99.gzzzzzzz  <  0.2.0-1                and the next release beats them all
```

!!! warning "Why not the commit hash alone"
    Stamping `1+g81226ee` was tried first and is subtly broken: dpkg compares
    those suffixes lexically, so ordering follows the alphabet rather than
    time. Measured on real builds:

    ```
    0.1.0-1+ff1d3f5  >  0.1.0-1+03fa7eb    the later build sorts LOWER
    ```

    Roughly half of upgrades between development builds would be refused as
    downgrades. The height fixes it because it only ever increases.

## Why the height goes after the hyphen, not in the third digit

The obvious alternative is to let commit height *be* the third digit —
`0.1.12` for twelve commits past `0.1` — which is what Nerdbank.GitVersioning
does. It does not fit here, for one reason of meaning and one of mechanism.

The third digit is **patch**, not a build counter. It is a claim that a
release happened. Spending it on height means `0.1.12` asserts twelve patch
releases that nobody made, and — worse — leaves no digit to say *this is a
bug-fix release of 0.1.0*, because the position that would have said it is
already occupied by an accident of how many commits were on the branch.

The .NET pattern works because assembly versions are
`Major.Minor.Build.Revision`: four fields, no semver contract, and the third
is literally named Build. Cargo has three fields and a contract about what
they mean.

Debian already has the field for this. A package version is
`upstream_version-debian_revision`, and the revision means *which build of
that upstream version this is*. That is exactly what commit height is, so it
goes there:

```
0.1.0    -    1+12.g81226ee
└─ the release       └─ which build of it
   this aims at
```

Both parts then say something true at once: the release this is working
towards, and how far past it this particular build sits. Neither has to lie
about the other.

The mechanical reason is smaller but decides it anyway — see
[below](#why-not-commit-height-in-cargotoml).

## Building one by hand

The same scheme, for a build you want to put on a machine:

```bash
cargo build --release
cargo deb --no-build --deb-revision "1+$(git rev-list --count "$(git describe --tags --abbrev=0)..HEAD").g$(git rev-parse --short HEAD)"
```

Then `dpkg-query -W suede` on the appliance tells you exactly which commit is
installed, and `apt-get install ./suede_*.deb` upgrades rather than refusing.

## Why not commit height in `Cargo.toml`

This is the one place the .NET pattern does not transfer. Nerdbank.GitVersioning
rewrites the version at build time from `version.json` plus commit height, and
the equivalent here would mean rewriting `Cargo.toml` in CI: Cargo requires a
literal version string and will not compute one.

That churns `Cargo.lock` on every build and makes a local `cargo build`
disagree with CI about what it just produced. The Rust-idiomatic split is the
one above — the manifest states intent, the build records identity — and it
gets you the same traceability without a generated manifest.
