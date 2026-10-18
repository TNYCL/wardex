# Exposing the MCP endpoint

Wardex listens on `127.0.0.1` by default. To let remote MCP clients reach it,
put a tunnel in front of it. Do **not** bind it to a public interface.

## Token first

A tunnel forwards remote traffic to `127.0.0.1`, so the daemon cannot tell it
apart from local traffic. That is why the HTTP/MCP endpoint **always** requires
a bearer token, and the daemon refuses to start HTTP without one.

```sh
openssl rand -hex 32   # put the result in WARDEX_TOKEN (EnvironmentFile)
```

Clients send `Authorization: Bearer <token>`. Rotate the token by changing the
env file and restarting the daemon.

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
extra layer.

## Tailscale Funnel

```sh
tailscale funnel --bg 7878
```

The MCP URL is `https://<machine>.<tailnet>.ts.net/mcp`. Use `tailscale serve`
instead of `funnel` if only your tailnet needs access.
