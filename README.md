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

Connect the two Macs with a Thunderbolt cable. In System Settings > Network,
check that **Thunderbolt Bridge** is enabled on both.

On the receiving Mac:

```sh
tf recv ~/Downloads
```

It prints its address and a three-word phrase such as `apple-river-stone`.

On the sending Mac:

```sh
tf send ~/Movies/master.mov ~/Photos/2019
```

Type the phrase when prompted, or pass it with `--token apple-river-stone`.
The phrase is fresh every run, so there is nothing to set up or store. Both
sides exit when the transfer completes; nothing runs in the background.

`caffeinate` keeps both Macs awake until `tf` exits. Closing a laptop lid on
battery still sleeps the machine, so keep the lids open.

## Speed

`tf` prints live throughput. Measured between an M5 and an M4 MacBook Pro — a
mixed pair, so the link negotiated Thunderbolt 4 at 40 Gb/s — sending 29.7 GB
of real media files:

- default (BLAKE3-verified): **2.3 GB/s**, limited by one hashing core per side
- `--no-verify`: **3.7 GB/s**, limited by disk and link; sender CPU near zero

Even a Thunderbolt 4 link outruns the hasher; a TB5-to-TB5 pair (80 Gb/s) is
unmeasured and can only widen the gap.

Jumbo frames (MTU 9000 on both Macs) made no measurable difference on that
pair. To try them anyway: macOS refuses `mtu 9000` on `bridge0` directly — set
each member interface first (`ifconfig bridge0 | grep member`), then
`bridge0`, on both Macs. The setting does not survive a reboot, and `tf` never
changes network settings itself.

## What it does not do

By design, for now:

- **No encryption.** Traffic is plaintext. The receiver binds only the
  Thunderbolt interface and requires the phrase, but anyone who can reach that
  interface with the phrase can write files. Fine for a cable between two
  machines you own; not fine for anything else.
- **No extended attributes.** Permission bits are preserved (setuid/setgid
  dropped; directories keep owner `rwx` so their contents can be written), and
  mtime to one-second resolution. Xattrs, ACLs and resource forks are not, so
  `.app` bundles, Photos libraries and quarantine-flagged files will arrive
  subtly broken.
- **No resume.** A part-sent file starts from zero, including one stopped by a
  verification failure. Whole files already at the destination are skipped on
  the retry (see Repeat transfers), so a big tree does not start over.
- **No pull mode.** Push only.

## Repeat transfers

Each run opens with a manifest: the sender lists what it is about to send, the
receiver answers with the files it already holds, and those never cross the
wire. A file counts as held when both its size and its mtime match — the same
quick check rsync makes by default. `tf` stamps the sender's mtime on every
file it writes, so a second run over an unchanged tree moves nothing but the
directory entries, and says how many files it skipped.

Contents are not compared. A destination file edited to the same size and
timestamp is left alone; delete it to force a fresh copy.

Files are replaced atomically. The receiver writes to `.name.tf-partial`
beside the target and renames over the real name only once the last byte has
arrived and verified, so an interrupted transfer costs the temp file and never
the copy that was already there. Names too long to carry the prefix and suffix
(over 243 characters) are written in place instead, and those can still be
lost to an interrupted run.

## Content verification

Every file and symlink is verified end to end with BLAKE3. The sender hashes
each chunk as it leaves the disk and puts the 32-byte digest on the wire after
the payload; the receiver hashes the same bytes as they arrive and compares.
Neither side reads the data twice. A mismatch discards the partial file, fails
with the offending path and withholds the acknowledgement, so the sender
reports failure too.

Verification is on by default and caps throughput at one BLAKE3 core (about
2.5 GB/s). `tf send --no-verify` turns it off for that run — no hashing on
either end, no digests on the wire; the receiver prints
`content verification disabled by sender`. There is no receiver-side flag.

The trade-off is real: TCP's 16-bit checksum is weak, so a `--no-verify`
transfer can deliver silently corrupted bytes and still report success. Leave
it on unless the link is genuinely faster than the hasher — on the 40 Gb/s
link above it bought 39%; on slower links, or against a slow destination
disk, it buys nothing.

Both Macs must run the same protocol version (currently v3, which added the
manifest exchange). Only the receiver parses a handshake, so only the
receiving Mac names a mismatch (`sender speaks protocol v1, this build speaks
v3`); the sending Mac just sees the connection close, as with a mistyped
phrase.

## Durability

By default the acknowledgement means every byte arrived, verified and was
written — in the page-cache sense. The kernel has the data; the drive may not.
A power cut straight after a successful transfer can still lose some or all of
it.

`tf recv --durable` withholds the acknowledgement until each regular file has
been flushed with `F_FULLFSYNC` (which asks the drive to empty its own write
cache) and the destination root directory has been flushed too. A file is
flushed after its mtime is stamped, so the timestamp is persisted with the
contents. Where the filesystem does not support `F_FULLFSYNC`, `tf` falls back
to `fsync` and prints one warning. Any other flush failure discards the
partial file and fails the transfer, the same as a write error.

`--durable` is no defence against hardware, firmware or filesystem corruption,
or a drive that lies about its cache. Symlinks are not flushed, and
directories below the destination root are not individually flushed, so an
intermediate directory entry can still be lost even though the file it named
was flushed.

The cost scales with file count, not bytes: 10,000 × 1 MiB files went from
6.1 s to 40.4 s; one 16 GiB file was unchanged.

## Options

```
tf recv [DIR]              Receive into DIR (default: current directory)
  --port <PORT>            Fixed TCP port (default: any free port)
  --durable                Flush files and the destination directory to
                           permanent storage before acknowledging
  --stats                  Print transfer diagnostics to stderr when done

tf send <PATHS>...         Send files or directories
  --peer <ADDR:PORT>       Skip discovery, connect directly
  --token <PHRASE>         The phrase shown by tf recv (prompted if omitted)
  --no-verify              Skip BLAKE3 verification: no hashing, no digests on
                           the wire. Faster only on links above ~2.5 GB/s, and
                           corruption then goes undetected
  --stats                  Print transfer diagnostics to stderr when done
```

## Security notes

- The TCP listener binds the `bridge0` address only. It never binds `0.0.0.0`
  and never falls back to another interface — if Thunderbolt Bridge has no
  address, `tf` refuses to run.
- Each `tf recv` run picks a fresh three-word phrase (about 24 bits of
  entropy) and accepts exactly one connection, so a wrong guess ends the
  session. The phrase is hashed to the 32-byte wire token on both sides and
  compared in constant time.
- The UDP discovery responder binds the wildcard address, because BSD sockets
  bound to a unicast address do not receive broadcasts. It ignores probes from
  outside the bridge subnet, and can only reply with an address; it cannot
  cause a write.
- Neither end blocks forever on a peer that goes quiet: the handshake must
  arrive within 5 seconds and the stream must not stall for more than 60,
  after which the transfer fails and any partial file is deleted.
- Sender-supplied paths are rejected if absolute, containing `..` or
  containing a NUL. Every path component is then opened with `O_NOFOLLOW`, so
  a transmitted symlink cannot redirect writes outside the destination.
