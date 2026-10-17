# Behind a reverse proxy

Suede has no user accounts and no TLS. On a trusted production LAN that is fine. On a network you share with people you do not control, put something in front that does both jobs, and keep Suede for what it is good at.

The shape this page describes:

| Who | How they reach Suede | What proves who they are |
|---|---|---|
| People using the web UI | Through the proxy, over HTTPS | They sign in at the proxy |
| Machines using the API (show control, scripts) | Through the proxy, or directly if you choose | The bearer token, sent as `Authorization: Bearer ...` |
| Content running on the appliance | The [local listener](configuration.md#local-listener) | It is on this machine |

The proxy adds the token to requests from people who signed in. So Suede sees a token on every request and needs no idea who anyone is. That is also the limit: everyone who can sign in acts as the same caller. Suede cannot tell your operators apart, and its logs show the proxy, not a person. Per-person API keys are not built.

If you only need to keep strangers out of an otherwise open network, the [security posture table](getting-started.md#security-posture) lists cheaper options: a VPN, or an SSH tunnel.

## Set Suede up for it

Three changes, none of which the proxy can make for you.

**1. Bind Suede to loopback.** The proxy runs on the same machine and forwards to `127.0.0.1:9088`:

```toml
# ~/.config/suede/suede.toml
bind = "127.0.0.1:9088"
```

Do this even though the proxy would work without it. While the main listener is bound to a routable address it trusts loopback callers with app heartbeats, and a proxy on the same machine forwards from loopback. Anyone the proxy lets through could then post heartbeats as if they were page content on the appliance. Bound to loopback, the main listener refuses those routes outright and only the [local listener](configuration.md#local-listener) answers them.

**2. Set a token.** It is what the proxy adds, and what machines carry:

```toml
token = "a-long-random-string"
```

**3. Close the firewall port.** With Suede on loopback nothing needs `9088` open. Open `443` (and `80`, if the proxy redirects) instead, in whichever of `ufw` or `firewalld` the machine runs.

Provisioning does the first and third and writes the second for you:

```bash
sudo /usr/share/suede/provision.sh --local-only yes --token generate
```

`--local-only yes` binds to loopback and skips the firewall rule (a bare `--local-only` means the same). `--token generate` writes a random token into `suede.toml`, mode 600, and prints it once on the terminal. Copy it to wherever the proxy reads its secrets from. It is safe to run on a machine that was provisioned before, and it will not replace a token that is already there without a yes at a terminal. Leave either flag off and, at a terminal, provisioning asks the question instead; see [Getting started](getting-started.md) for all three. If the port was opened earlier, close it yourself, for example `sudo ufw delete allow 9088/tcp`.

Restart the daemon after editing the file by hand: `systemctl --user restart suede`.

## What the proxy must do

**Authenticate people before it adds the token.** The token goes on only for requests that passed your sign-in, and never on ones that did not. A rule that adds it to everything turns the proxy into an open door.

**Pass requests that already carry a bearer token through unchanged.** Those are machines. Suede checks their token itself, so passing them through grants nothing a wrong token would not also be refused. Match on `Bearer`, not on any `Authorization` header: the `Basic` header a browser sends after signing in is yours to consume, not Suede's. This route is safe only because Suede requires a token, which is why setting one is step 2 above. Without a token, anyone could add a `Bearer` header, skip the sign-in, and reach an API that checks nothing.

**Replace, not append.** For a signed-in person, the `Authorization` header Suede receives must be the token and nothing else. That also keeps the person's own password from reaching Suede.

**Never proxy the app-facing paths.** These are for content on the appliance, not people:

- `/capability-check/{id}`
- `/api/v1/capability-check/{id}/result`
- `/api/v1/apps/{id}/heartbeat`

Suede already answers `404` for them on a loopback main listener. Refusing them at the proxy as well keeps that true if `bind` is changed later. And never point the proxy at the local listener (port `9089` by default).

**Do not buffer or compress `/api/v1/events`.** That is the server-sent event stream the web UI listens to. Compression and response buffering hold events back until a buffer fills, and a UI that gets nothing for a minute looks like an appliance that has stopped. Turn buffering off for the whole site, or at least for that path, and leave it out of any compression rule. Do not set a short read timeout either: the stream is quiet between events by design.

**Forward the real `Host`, or list the name Suede sees.** [`allowed_hosts`](configuration.md#cross-site) refuses requests whose `Host` is not in its list, as a defense against DNS rebinding. If you set it, include the public name people type. Most proxies forward the original `Host` by default, and Caddy does. A proxy that sends `127.0.0.1:9088` instead passes as loopback and needs nothing listed.

Cross-site protection needs nothing special. The web UI sends `Sec-Fetch-Site: same-origin`, which Suede accepts whatever `Host` the proxy forwards. Read [Cross-site request protection](configuration.md#cross-site) for what it does and does not cover.

## Example: Caddy

[Caddy](https://caddyserver.com/) is a single binary with automatic HTTPS. This Caddyfile follows every rule above: it signs people in with `basic_auth`, adds the token only after that, passes bearer requests through unchanged, refuses the app-facing paths, and neither compresses nor buffers the event stream. It ships with the package at `/usr/share/doc/suede/examples/Caddyfile`, and has been run against a stand-in for Suede to confirm what Suede receives in each case.

```caddyfile
--8<-- "examples/Caddyfile"
```

Suede does not install Caddy and never will. To use it:

1. Install Caddy from [its own instructions](https://caddyserver.com/docs/install) and copy the file to `/etc/caddy/Caddyfile`.
2. Generate a password hash with `caddy hash-password` and put it in the `basic_auth` block in place of the placeholder. The placeholder is deliberately not a hash, so every sign-in is refused until you replace it: a working example hash would be a working password on any machine where it was left in.
3. Give Caddy the token. `{env.SUEDE_TOKEN}` is read from Caddy's environment, so set `SUEDE_TOKEN` there (the file's comments show a `systemctl edit caddy` override). Keep the token out of the Caddyfile itself, which is more often shared and committed.
4. `caddy validate --adapter caddyfile --config /etc/caddy/Caddyfile`, then reload.

With `tls internal`, Caddy signs with its own certificate authority, which browsers do not trust until you install its root certificate. That is the right start on a LAN with no public name. With a public name that resolves to this machine, delete that line and Caddy gets a trusted certificate by itself.

## Other software

Nothing here is specific to Caddy. Any proxy that can authenticate a request, set a request header and leave a response unbuffered can do the job. These are the usual ones. Check each project's own documentation for current syntax; none of the following has been run against Suede.

| Software | How it fits |
|---|---|
| nginx | `auth_request` to an authentication service or `auth_basic` for sign-in, `proxy_set_header Authorization "Bearer ..."` to add the token, and `proxy_buffering off` for the event stream. Use a `map` on `$http_authorization` to pass requests that already carry `Bearer` through unchanged |
| Traefik | A `forwardAuth` middleware or `basicAuth` for sign-in, and a `headers` middleware with `customRequestHeaders` to add the token |
| oauth2-proxy | Authenticates people against an identity provider (Google, GitHub, OIDC). It sits beside nginx or Traefik, which still add the token |
| Authelia, Authentik | Single sign-on with groups and two-factor. They authenticate for a proxy through forward auth; the proxy adds the token |
| Tailscale Serve | Reachable only from your tailnet, which is the authentication. Bound to loopback, it forwards to Suede. As far as we know it cannot add a request header, so it suits an appliance with no token set, where the tailnet is the whole boundary |
| Cloudflare Access | Authenticates at Cloudflare's edge before traffic reaches you, usually through `cloudflared` on the appliance. Adding the token needs a request header rule at Cloudflare or a proxy such as Caddy between `cloudflared` and Suede |

If the software in front of Suede cannot add a header, put Caddy or nginx between them. The pattern stays the same.

## When it does not work

- **The web UI shows a page saying the appliance requires an API token.** The UI reached Suede, but the request carried no token. The proxy is not adding it, or you opened the appliance directly rather than through the proxy. Check that `SUEDE_TOKEN` is set in the proxy's environment and matches `token`.
- **Every request is `401` through the proxy but works with curl directly.** The token the proxy adds differs from the one in `suede.toml`. A trailing newline in an environment file is a common cause.
- **Events stop arriving; the UI shows stale values until reloaded.** Something is buffering or compressing `/api/v1/events`, or a timeout is cutting the stream. See above.
- **`403` with a message about `Host`.** `allowed_hosts` is set and does not list the name people use.
- **An app's heartbeats fail after you moved to loopback.** Content that hardcodes `http://127.0.0.1:9088/api/v1/apps/<id>/heartbeat` must use the `{heartbeatUrl}` it is given. See [Local listener](configuration.md#local-listener).

The `api-reachability` health check passes on a loopback bind and links here. It cannot tell a proxied appliance from a merely local one, so it does not try.
