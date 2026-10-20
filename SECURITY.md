# Security policy

## Reporting a vulnerability

Report it privately. On the repository's
[Security tab](https://github.com/gameshowpro/Suede/security), choose
**Report a vulnerability**. Please do not open a public issue or pull request
for it.

If you cannot use that, contact the maintainer through
<https://barjonas.com/#contact>.

## Supported versions

While Suede is 0.x, only the latest release gets security fixes. Older 0.x
releases do not. While the major version is 0, a minor release can be
breaking; see
[Versioning](https://suede.gameshow.pro/developer/versioning/#while-the-major-is-0).

To upgrade, run `sudo apt update && sudo apt install suede` on a machine that
uses the project's signed apt repository, or install the newest `.deb` from the
[releases page](https://github.com/gameshowpro/Suede/releases). Both are
described under
[Install](https://suede.gameshow.pro/getting-started/#install).

## What counts as a vulnerability

Suede's expected deployment is a trusted production LAN, and it is
unauthenticated by default and listens on every interface. The
[Security posture](https://suede.gameshow.pro/getting-started/#security-posture)
section says what to do for each situation. Suede has no TLS and no user
accounts of its own; a
[reverse proxy](https://suede.gameshow.pro/reverse-proxy/) provides both.

The protections Suede does provide are described in the documentation:

- The optional bearer token, and what the web UI does in token mode.
- Refusal of state-changing requests that a browser sends for a page on another
  site, with `allowed_origins` to admit trusted pages and `allowed_hosts` as a
  defense against DNS rebinding. See
  [Cross-site request protection](https://suede.gameshow.pro/configuration/#cross-site).
- The [local listener](https://suede.gameshow.pro/configuration/#local-listener),
  which answers app-facing routes only to callers on the appliance itself.
- [`allowed_programs`](https://suede.gameshow.pro/configuration/#allowed-programs),
  which limits what Suede will launch.

A flaw that defeats one of these is in scope. For example, a way to make the
appliance run a program that is not allowed, or to drive it from another site
despite the cross-site protection.

Behavior the documentation describes as the default is not a vulnerability. In
particular, an open API on a network the operator chose to trust, with no token
set, is expected. So is forwarding the port to the internet against the
documented advice.

## How reports are handled

Suede has a single maintainer. Reports are acknowledged and fixed on a
best-effort basis, and no response time is promised. A fix ships as a
release whose commit message says it is a security fix, as the commit for
release 0.2.1 did. Reporters who want credit are credited there.

## Supply chain

Releases carry build provenance attestations, and the apt repository is signed.
See
[Provenance](https://suede.gameshow.pro/developer/deployment/#provenance) for
how to verify a package.
