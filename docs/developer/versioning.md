# Versioning

Two numbers, decided in two different ways. The release version is a
judgement and lives in `Cargo.toml`; everything else about a build is derived
from git and never typed by anyone.

## Before you commit

**Nothing.** Ordinary commits do not touch the version. Push as often as you
like; CI builds every push and publishes nothing, because the tag it would
create already exists.

**Bump `Cargo.toml` only when you intend to release**, which is the one
decision a person has to make:

| Change | Bump | Because |
|---|---|---|
| A field is removed or renamed, or an existing document stops being accepted | major | Somebody's saved configuration will be refused |
| A new field, endpoint, or capability | minor | Existing documents still apply |
| A fix that changes no shape | patch | Nothing a client sees moves |

Suede is an appliance daemon, not a library, so nobody resolves against its
version as a dependency. What the number is for is telling an operator
whether an upgrade can break the configuration they already have — which is
why the table is about *documents*, not about code.

Pushing that bump to `main` is what makes a release: CI tags `v{version}`,
builds both architectures, and attaches the packages. Pushing anything else
builds and tests but publishes nothing.

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
