# thunderflash

Fast file transfer between two Macs joined by a Thunderbolt cable.

macOS already exposes the cable as an IP interface (`bridge0`, "Thunderbolt
Bridge"). `tf` uses it directly: one pipelined TCP connection, no SSH crypto in
the way, no per-file round trips.

## Install

```sh
cargo install --path .
```

## Use

Connect the two Macs with a Thunderbolt cable. Check System Settings > Network
that **Thunderbolt Bridge** is present and enabled on both.

On the Mac receiving:

```sh
tf recv ~/Downloads
```

It prints its address and a token. Copy that token to the other Mac once:

```sh
tf token 7f3a...
```

Then send:

```sh
tf send ~/Movies/master.mov ~/Photos/2019
```

Both sides exit when the transfer completes. Nothing runs in the background.

## Speed

`tf` prints live throughput. On a Thunderbolt 4 link expect roughly
1.2–2.5 GB/s. If the number is well below that, try jumbo frames on **both**
Macs:

```sh
sudo ifconfig bridge0 mtu 9000
```

This does not persist across reboots. `tf` prints a reminder when it sees an
MTU of 1500 but never changes network settings itself.

## What it does not do

By design, for now:

- **No encryption.** Traffic is plaintext. The receiver binds only the
  Thunderbolt interface and requires a shared token, but anyone who can reach
  that interface with the token can write files. Fine for a cable between two
  machines you own; not fine for anything else.
- **No extended attributes.** Mode and mtime are preserved; xattrs, ACLs, and
  resource forks are not. `.app` bundles, Photos libraries, and
  quarantine-flagged files will arrive subtly broken.
- **No resume.** An interrupted transfer starts over.
- **No content hashing.** Truncated files are detected by length and deleted,
  which catches a dropped connection but not silent corruption.
- **No pull mode.** Push only.

## Options

```
tf recv [DIR]              Receive into DIR (default: current directory)
  --port <PORT>            Fixed TCP port (default: any free port)

tf send <PATHS>...         Send files or directories
  --peer <ADDR:PORT>       Skip discovery, connect directly
  --token <HEX>            Override the stored token

tf token [VALUE]           Print the local token, or store one
```

## Security notes

- The TCP listener binds the `bridge0` address only. It never binds `0.0.0.0`
  and never falls back to another interface — if Thunderbolt Bridge has no
  address, `tf` refuses to run.
- The UDP discovery responder binds the wildcard address, because BSD sockets
  bound to a unicast address do not receive broadcasts. It ignores probes from
  outside the bridge subnet, and can only reply with an address; it cannot
  cause a write.
- Sender-supplied paths are rejected if absolute, containing `..`, or
  containing a NUL. Every path component is then opened with `O_NOFOLLOW`, so a
  transmitted symlink cannot redirect writes outside the destination.
