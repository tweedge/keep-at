# Running keep-at behind a VPN

**This is a general guide, not a tested one.** It's based on research into how BitTorrent, VPN port forwarding, and Linux policy routing actually behave, not on running keep-at against a live VPN in this project's own testing (there wasn't one available). VPN provider features and pricing change often - verify anything provider-specific against that provider's current documentation before relying on it.

## Should you use one?

### Reasons to

* **IP privacy.** keep-at connects to swarms for torrents it selected automatically, not ones you personally reviewed. Your IP is visible to every peer in every swarm you're in, plus anyone passively monitoring those swarms. A VPN puts its IP there instead of yours.
* **ISP interference.** Some ISPs throttle or outright block BitTorrent traffic by protocol signature, independent of what's actually being transferred. A VPN tunnel hides this from your ISP.
* **Defense in depth.** keep-at already applies a keyword blocklist and a moderation delay (see [DESIGN.md](DESIGN.md)) before touching anything, but no automated filter is perfect. A VPN is one more layer between a catalog mistake and your real-world identity.

### Reasons not to, or to think carefully first

* **Port forwarding is the real cost.** BitTorrent works best when peers can connect to you directly ("connectable"), not just when you connect out to them. Most VPN providers put you behind their own NAT with no way to open an inbound port at all - **NordVPN, ExpressVPN, and Surfshark don't offer port forwarding**, and **Mullvad removed it in 2023**. Without a forwarded port, keep-at can still download and seed, but only via outbound connections, which measurably hurts both speed and how useful you are to the swarm - somewhat working against the whole point of running keep-at, which is to reliably keep things seeded. As of this writing, providers that do support port forwarding include **ProtonVPN** (Plus plan, WireGuard, on P2P-tagged servers only), **Private Internet Access**, **TorGuard**, and a few smaller providers with paid add-ons. Verify current support directly with whichever provider you're considering; this list changes.
* **Throughput overhead.** Encryption and routing through a third party add latency and can cap bandwidth well below your raw connection, especially on budget providers. keep-at's job is moving a lot of data over time; a slow tunnel works directly against that.
* **A misconfigured kill switch is worse than no VPN.** If the tunnel drops and nothing stops traffic from falling back to your normal connection, you get a brief, silent leak instead of the privacy you thought you had. If you do this, set it up properly (see below) rather than assuming the default is safe.
* **Double NAT.** If you're already behind CGNAT (common on some ISPs and most mobile connections), stacking a VPN's NAT on top makes an already-bad connectivity situation worse, not better.

There's no universally correct answer here. If you're running keep-at on a home connection with no P2P restrictions and don't mind your IP being visible to swarms, skipping the VPN entirely is a perfectly reasonable choice, and keeps things simpler and faster.

## Docker: gluetun

[gluetun](https://github.com/passteque/gluetun) is a VPN client that runs as its own container and routes other containers' traffic through it, with a built-in firewall-based kill switch - if the tunnel drops, gluetun's firewall blocks traffic rather than letting it fall back to the host's normal route. It supports both OpenVPN and WireGuard across many providers, and has [native port-forwarding support](https://github.com/qdm12/gluetun-wiki/blob/main/setup/advanced/vpn-port-forwarding.md) for the providers that offer it (notably ProtonVPN and PIA).

The pattern is to run gluetun as a container, then attach keep-at's container to gluetun's network namespace instead of giving it its own:

```yaml
services:
  gluetun:
    image: qmcgaw/gluetun
    cap_add:
      - NET_ADMIN
    devices:
      - /dev/net/tun:/dev/net/tun
    ports:
      - 37550:37550/tcp # keep-at's BitTorrent port - see below
      - 37550:37550/udp
    volumes:
      - ./gluetun:/gluetun
    environment:
      - VPN_SERVICE_PROVIDER=protonvpn # or your provider
      - VPN_TYPE=wireguard
      - WIREGUARD_PRIVATE_KEY=...
      - WIREGUARD_ADDRESSES=...
      - VPN_PORT_FORWARDING=on # only if your provider supports it
    restart: unless-stopped

  keep-at:
    image: ghcr.io/tweedge/keep-at:latest
    network_mode: "service:gluetun" # keep-at has no network of its own
    depends_on:
      - gluetun
    volumes:
      - ./data:/data
      - ./storage:/storage
    command: ["--data-dir", "/data", "--storage", "/storage", "--storage-limit", "500G", "--port", "37550"]
    restart: unless-stopped
```

Notes:

* Ports are published on the **gluetun** service, not keep-at's, since keep-at has no network stack of its own once `network_mode: "service:gluetun"` is set.
* If your provider supports port forwarding and gluetun negotiates a port for you, pass that port to keep-at with `--port` (or `port` in a config file) so its BitTorrent listener matches what's actually forwarded.
* See [gluetun's wiki](https://github.com/qdm12/gluetun-wiki) for provider-specific environment variables - they vary significantly and are outside keep-at's scope to document.

## Service-level (non-Docker): WireGuard

Without Docker, the equivalent is running WireGuard as a system-level interface and deciding how much of the system's traffic to route through it.

### Whole-system tunnel with a kill switch

The straightforward option: `wg-quick` brings up a `wg0` interface and routes all traffic through it, and a proper kill switch means traffic stops entirely if the tunnel drops rather than silently leaking over your normal connection.

An application-level "kill switch" (a client noticing the tunnel dropped and then reacting) leaves a window - however small - where traffic can leak before it reacts. The robust version blocks at the firewall level, unconditionally, and only opens up while the tunnel is confirmed up. The common pattern (as of this writing) uses `PostUp`/`PreDown` hooks in `wg0.conf` together with `fwmark`-based policy routing and a "blackhole" default route that's only removed while the tunnel is actually up:

* `PostUp`: mark and route the VPN's own handshake traffic around the tunnel (so it can reach the VPN endpoint at all), route everything else through `wg0`, and remove the blackhole route.
* `PreDown`: reinstate the blackhole route before the interface goes down, so there's no gap where traffic could fall back to the normal route.

This is real firewall configuration, and the exact commands depend on your distribution's `iptables`/`nftables` setup and your VPN provider's endpoint details - search for "WireGuard kill switch fwmark blackhole route" for current, detailed walkthroughs rather than relying on a fixed set of commands here, since the specifics (and the tools' own recommended syntax) do shift over time.

### Routing only keep-at's traffic (optional, more surgical)

If you'd rather not slow down the whole machine, Linux can route traffic by which user or process generated it, using the same `fwmark` + policy-routing mechanism as above but scoped to a UID instead of "everything":

1. Run keep-at as its own dedicated user (systemd's `User=` in the unit `service install` generates already supports this via `--user`).
2. Add an `iptables`/`nftables` rule matching packets owned by that user (the `owner`/`socket` match module) and mark them.
3. Add a policy routing rule (`ip rule add fwmark ... table ...`) that sends only marked packets through the WireGuard interface; everything else keeps using the normal default route.

This confines the VPN's overhead to keep-at specifically, at the cost of more setup and more moving parts to get wrong. It's a reasonable trade for a server also doing other things where you don't want all of its traffic tunneled.

Whichever approach you take, verify it actually works before trusting it: check your apparent IP while keep-at is running (e.g. against a "what's my IP" service) and confirm it's the VPN's, not your own, and test what happens when you deliberately stop the WireGuard interface while keep-at is active.

## Either way: passing the port through

Whatever VPN setup you use, if you get a forwarded port, tell keep-at about it with `--port` (or `port` in a config file) so its BitTorrent listener matches. A VPN tunnel with no forwarded port still works, just less effectively - see "Reasons not to" above.
