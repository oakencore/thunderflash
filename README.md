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

It prints its address and a three-word phrase, like `apple-river-stone`.

On the sending Mac:

```sh
tf send ~/Movies/master.mov ~/Photos/2019
```

Type the phrase when prompted (or pass it with `--token apple-river-stone`).
The phrase is fresh every run, so there is nothing to set up or store. Both
sides exit when the transfer completes. Nothing runs in the background.

Both Macs are kept awake during the transfer via `caffeinate`; the assertion
is released as soon as `tf` exits. Closing a laptop lid on battery still
sleeps the machine, so keep the lids open.

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
  Thunderbolt interface and requires a three-word phrase, but anyone who can
  reach that interface with the phrase can write files. Fine for a cable
  between two machines you own; not fine for anything else.
- **No extended attributes.** Permission bits are preserved (setuid/setgid are
  dropped, and a directory always keeps owner `rwx` so its contents can be
  written); mtime is preserved to one-second resolution. Xattrs, ACLs, and
  resource forks are not. `.app` bundles, Photos libraries, and
  quarantine-flagged files will arrive subtly broken.
- **No resume.** An interrupted transfer starts over.
- **No resume after a failed check.** A file whose contents do not verify is
  deleted and the transfer stops; rerun it.
- **No pull mode.** Push only.

## Content verification

Every file and symlink is verified end to end with BLAKE3. The sender hashes
each chunk as it comes off the disk and puts the 32-byte digest on the wire
right after the payload; the receiver hashes the same bytes as they come off
the socket and compares. Neither side reads the data twice.

A mismatch deletes the destination file, fails with the path that failed, and
withholds the acknowledgement, so the sender reports failure too.

Verification is on by default and costs throughput: BLAKE3 runs at about
2.5 GB/s per core, so it caps a transfer at that rate. `tf send --no-verify`
turns it off for that run: no hashing on either end, no digests on the wire.
On this hardware a 16 GiB localhost transfer measures 2.42 GB/s verified
against 3.91 GB/s with `--no-verify` (mean of three runs each).
The sender announces this in the handshake and the receiver prints
`content verification disabled by sender`; there is no receiver-side flag.

The trade-off is real: TCP's 16-bit checksum is weak, so a `--no-verify`
transfer can deliver silently corrupted bytes and still report success. Leave
it on unless the link is genuinely faster than the hasher — on anything up to
a TB4 cable (about 2.5 GB/s) verification is free, and only a TB5-class link
where the wire outruns one BLAKE3 core has anything to gain.

Both Macs must run the same protocol version (currently v2, which added the
digests). Only the receiver parses a handshake, so it is the receiving Mac that
prints the mismatch by name (`sender speaks protocol v1, this build speaks v2`)
and writes nothing. The sending Mac only sees the connection close, with no
version in the message — the same as for a mistyped phrase.

## Durability

By default, the receiver's acknowledgement means every byte arrived, was
verified against its BLAKE3 digest, and was written — in the page-cache sense.
The kernel has the data; the drive may not. A power cut immediately after the
transfer reports success can still lose some or all of it.

`tf recv --durable` adds one guarantee: the acknowledgement is not sent until
each regular file has been flushed with `F_FULLFSYNC` (which asks the drive to
empty its own write cache), and the destination root directory has been
flushed too, so the top-level names are on disk. A file is flushed after its
mtime is stamped, so the timestamp is persisted with the contents. On a
filesystem that does not support `F_FULLFSYNC`, `tf` falls back to `fsync` and
prints one warning to stderr saying full durability is unavailable. Any other
flush failure deletes that file and fails the transfer with no acknowledgement,
the same as a write error.

What `--durable` does not promise: it is no defence against hardware,
firmware, or filesystem corruption, or against a drive that lies about its
cache. Symlinks are not flushed, and directories below the destination root do
not get their own flush, so an intermediate directory entry can still be lost
even though the file it named was flushed.

The cost is a per-file flush, so it scales with file count, not bytes.
Localhost measurements: 10,000 × 1 MiB files, 6.1 s → 40.4 s (+567%); one
16 GiB file, 7.0 s → 7.1 s (no measurable change).

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
  --no-verify              Skip BLAKE3 content verification: no hashing, no
                           digests on the wire. Faster only on links above
                           ~2.5 GB/s, and corruption then goes undetected
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
  arrive within 5 seconds and the stream must not stall for more than 60, after
  which the transfer fails and any partial file is deleted.
- Sender-supplied paths are rejected if absolute, containing `..`, or
  containing a NUL. Every path component is then opened with `O_NOFOLLOW`, so a
  transmitted symlink cannot redirect writes outside the destination.
