# Guest networking, without NAT

How to give a librevm guest a working network on a kernel that has no `nf_nat`, no
`iptables`, no `nft`, and a Wi-Fi interface that refuses to be bridged.

## The problem, in order

Each of these was tried and ruled out. The reasons matter more than the conclusion.

**`iptables` / `nftables`.** The kernel has the netfilter *framework* installed --
`nf_conntrack.ko`, `x_tables.ko` and the `xt_*` helpers are all in `/lib/modules` -- but
not NAT itself. Building it fails on two separate counts:

* `IP_NF_NAT` and the whole legacy `iptables` family depend on `IP_NF_IPTABLES_LEGACY`,
  which depends on `NETFILTER_XTABLES_LEGACY`, which is `depends on !PREEMPT_RT`. This
  kernel is PREEMPT_RT, so that path is closed by the kernel's own configuration.
* `nftables` does not go through that path and its options *do* configure, but
  `nf_tables.ko` needs `nfnetlink`, whose symbol it cannot resolve -- `CONFIG_NETFILTER_NETLINK`
  is a `bool`, the running kernel was not built with it, and a module cannot supply it.
  Enabling it means rebuilding the kernel.

So kernel NAT would mean a new kernel image, and therefore touching boot. That is the one
thing worth avoiding.

**A bridge over Wi-Fi.** `ip link set wlp1s0 master br0` is refused outright:

```
Error: Device does not allow enslaving to a bridge.
```

The same limitation Android has, and the reason DroidVM ships a userspace router at all.

**`slirp4netns`.** It works, and it configures a tap properly, but it cannot share it with
a VMM. A tap interface has one userspace end; slirp4netns wants it and crosvm wants it, and
whichever goes second gets `EBUSY`. slirp4netns is built for rootless containers, where the
container never opens the tap -- the kernel routes to it -- so the conflict never arises
there. With crosvm it always does.

**`passt` in vhost-user mode.** This is the tool designed for the job, and it gets
furthest: crosvm accepts the socket, the guest's virtio-net device appears, passt reports
`accepted connection`. Then passt dies with `SIGBUS`, and its own log says why:

```
You can start qemu with:
  ... -object memory-backend-memfd,id=memfd0,share=on,size=$RAMSIZE ...
```

vhost-user needs the guest's memory to be a *shared* object. A protected VM's memory is
lent to it, and lent memory is exactly what the host may not touch. That is the same wall
the virtio work ran into from the other side.

## What works: forwarding, not translating

Do not translate anything. Put the guest on the host's own subnet and let the host forward.

```
host's Wi-Fi :  192.168.1.185/24   (the uplink, gateway 192.168.1.1)
the tap      :  192.168.1.200/32   (host end)
                route 192.168.1.201/32 dev librevm-tap
the guest    :  192.168.1.201/24, gateway 192.168.1.1
```

The guest's address is valid on the house network, so the router answers it directly and
nothing needs rewriting. The host only has to carry packets between its two interfaces.
This is what DroidVM calls a *pseudo-bridge*, and it is why it needs no NAT on a phone
either.

Three details are the difference between working and not, and each was found the hard way:

* **The `/32` on the tap.** `ip addr add 192.168.1.200/24 dev tap` installs a second route
  for the whole subnet, and the host then sends its own traffic into an interface with
  nothing behind it -- the symptom is that the *host* loses the router, not the guest.
* **`proxy_arp` on both interfaces.** The guest ARPs for the router and that request
  arrives on the tap, so the tap has to answer it; the Wi-Fi side has to answer for the
  guest so the router can reply.
* **`arp_ignore = 0`.** The default of 1 means "answer only for addresses actually on this
  interface", which suppresses precisely the proxy answers above. With it left at 1 the
  guest sees no ARP reply and the neighbour table stays empty.

Everything else is one `ip_forward`.

## Using it

```sh
sudo librevm net enable demo          # turn it on for a machine, and set the host up
sudo librevm net status               # is the tap there?
sudo librevm net setup --index 1      # host side only, for a second machine
sudo librevm net teardown
sudo librevm net disable demo
```

`net enable` prints the guest-side configuration:

```sh
ip link set eth0 up
ip addr add 192.168.1.201/24 dev eth0
ip route add default via 192.168.1.1 dev eth0
```

There is no DHCP. The address is chosen by the host from its own subnet, so it is static
by construction; `--index` picks between machines, since two guests cannot share one.

The guest's kernel needs `CONFIG_VIRTIO_NET`. It also needs the balloon left off -- see
`virtio-on-gunyah.md` -- which librevm now does by default.

## Limits

* **The address is static.** A guest that expects DHCP will not get a lease. Slirp-style
  DHCP would need either the kernel NAT that is unavailable or one of the userspace
  options above, and none of them fit.
* **It is a layer-3 arrangement, not a layer-2 one.** Broadcast and multicast do not cross
  the tap, so anything relying on them -- mDNS, DHCP discovery, Wake-on-LAN -- will not
  work.
* **The guest is visible on the house network** with its own address, as a machine plugged
  in would be. That is the point, but it is worth knowing.
* **`--index` must differ per machine.** Two guests on one host share the subnet, and
  nothing stops them being given the same address.

## Verified

Through `librevm net enable` and an ordinary `librevm start`, from the guest's console:

```
--- ping the gateway ---
2 packets transmitted, 2 packets received, 0% packet loss
--- ping a public address ---
2 packets transmitted, 2 packets received, 0% packet loss
--- resolve a name ---
Name:    example.com
```

and a real download over TCP:

```
got 138612 bytes from the mirror
Origin: Debian
Label: Debian
Suite: stable
```
