# Exposing the MCP endpoint

Wardex listens on `127.0.0.1` by default. To let remote MCP clients reach it,
put a tunnel in front of it. Do **not** bind it to a public interface.
Tool reference and client setup: [`docs/mcp.md`](../docs/mcp.md).

## Token first

A tunnel forwards remote traffic to `127.0.0.1`, so the daemon cannot tell it
apart from local traffic. That is why the HTTP/MCP endpoint **always** requires
a bearer token, and the daemon refuses to start HTTP without one.

```sh
openssl rand -hex 32   # put the result in WARDEX_TOKEN (EnvironmentFile)
```

Clients send `Authorization: Bearer <token>`. Rotate the token by changing the
env file and restarting the daemon. Failed authentications are rate limited
(`http.auth_failures_per_minute`).

## Allow the public hostname

Tunnels pass the public hostname in the `Host` header, and `/mcp` rejects
hosts it does not know (`403`). Add yours to the config:

```toml
[http]
allowed_hosts = ["<your-hostname>"]
```

or set `WARDEX_ALLOWED_HOSTS=<your-hostname>` in the env file.

## Cloudflare Tunnel

```sh
cloudflared tunnel login
cloudflared tunnel create wardex
cloudflared tunnel route dns wardex <your-hostname>
```

`~/.cloudflared/config.yml`:

```yaml
tunnel: wardex
credentials-file: /path/to/<tunnel-id>.json
ingress:
  - hostname: <your-hostname>
    service: http://127.0.0.1:7878
  - service: http_status:404
```

Run it with `cloudflared tunnel run wardex` (or as a service). The MCP URL is
`https://<your-hostname>/mcp`. Cloudflare Access can be added in front as an
extra layer, as long as your MCP clients can pass its credentials (service
tokens are sent as headers).

## Tailscale Funnel

```sh
tailscale funnel --bg 7878
```

The MCP URL is `https://<machine>.<tailnet>.ts.net/mcp`; allow
`<machine>.<tailnet>.ts.net`. Use `tailscale serve` instead of `funnel` if
only your tailnet needs access.

## Check it

```sh
curl -s https://<your-hostname>/healthz
curl -s https://<your-hostname>/mcp \
  -H "Authorization: Bearer $WARDEX_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"overview","arguments":{}}}'
```

The first returns `{"ok":true,…}`; the second the overview as a tool result.
A `401` means the token is wrong, `403` that the hostname is not in
`allowed_hosts`.
