# Contributing

Bug reports, questions and pull requests are welcome.

## Licence of contributions, and why it is asked for

Suede is available under the [PolyForm Small Business License](LICENSE), and
separately under a commercial licence for organisations the PolyForm licence
does not cover. That only works while one party can grant both.

If a contribution stayed under its author's copyright with no grant, that code
could not be included in a commercial licence — which would mean either
removing it, tracking down every past contributor for permission, or quietly
selling something we had no right to sell. Projects discover this years later
and it is painful, so it is asked for at the start.

**By opening a pull request you agree that:**

1. You own the copyright in your contribution, or have permission from whoever
   does — most often an employer. If your employment contract assigns your
   work to your employer, get their sign-off first.
2. You grant Barjonas LLC a perpetual, worldwide, irrevocable, royalty-free
   licence to use, reproduce, modify, distribute and **sublicense** your
   contribution, under any licence terms, including commercial ones.
3. You keep your own copyright and remain free to do whatever else you like
   with your contribution.

You are not signing your work away. Point 2 is what lets the project be
offered under two licences at once; point 3 means you lose nothing.

Add a `Signed-off-by:` line (`git commit -s`) to confirm it.

## Before opening a pull request

Run the full check. It formats, lints, regenerates the OpenAPI snapshot,
and runs the tests and the smoke test:

```bash
./scripts/dev-check.sh fix
./scripts/dev-check.sh all
```

Notes on the code:

- Comments say *why*, not *what*. If a line needs explaining, it usually needs
  rewriting instead.
- Tests are named as sentences about behaviour, not after the function they
  exercise.
- Anything that changes what an operator sees — a divergence, a health check,
  a UI string — needs the documentation changed in the same commit.

## Reporting a problem

Say what the machine was doing, what you expected, and what happened. These
are usually the fastest things to include:

```bash
curl -s http://appliance:9088/api/v1/status        | python3 -m json.tool
curl -s http://appliance:9088/api/v1/system/checks | python3 -m json.tool
journalctl --user -u suede -n 100 --no-pager
```
