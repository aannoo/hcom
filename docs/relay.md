# Relay

Relay syncs hcom state across trusted devices through MQTT. It is designed for one operator's trusted machines, not for multi-tenant collaboration.

## Setup

Create a relay group:

```bash
hcom relay new
```

Join from another device:

```bash
hcom relay connect <token>
```

Check status:

```bash
hcom relay status
hcom relay daemon
```

Disable:

```bash
hcom relay off
hcom relay off --all
```

Use a custom broker:

```bash
hcom relay new --broker mqtts://host:port --password <broker-auth-secret>
hcom relay connect <token> --password <secret>
```

## What Syncs

Relay syncs:

- local instance snapshots.
- event batches.
- remote instance rows.
- remote RPC control results.

Remote instances appear with suffixes such as:

```text
luna:BOXE
```

Remote addressing works in normal commands:

```bash
hcom send @luna:BOXE -- Hello remote device
hcom f luna:BOXE --dir /home/riche/project
```

Remote fork requires `--dir`.

## Token and Broker Model

Current join tokens carry:

- relay ID.
- broker URL or default broker index.
- raw 32-byte PSK.

Legacy token formats without a PSK still parse, but `relay connect` rejects them for current secure relay use.

`--password` is broker authentication. It is distinct from the relay PSK. The broker password controls access to a private MQTT broker; it is not an additional encryption layer over hcom payloads.

## Encryption

Relay payloads are sealed before publication with XChaCha20-Poly1305.

Wire envelope:

```text
1 byte   suite
24 bytes nonce
8 bytes  timestamp, big-endian
N bytes  ciphertext including Poly1305 tag
```

Associated data binds:

```text
relay_id || topic || timestamp
```

This prevents a valid ciphertext from being replayed under a different relay ID, topic, or timestamp.

## Replay Guard

Relay uses two replay checks:

- clock skew window: 60 seconds for live envelopes.
- nonce LRU: sender plus nonce dedupe, with a bounded cache.

Retained MQTT state snapshots can be older than the live skew window, but they cannot roll back behind the newest state already accepted for that sender.

## Trust Model

Relay is full trust among enrolled devices.

Anyone with the token/PSK can:

- decrypt captured relay payloads.
- publish authenticated relay traffic.
- send messages to listening agents.
- trigger remote RPC behavior available to trusted peers.

If those agents can run tools, treat a compromised relay member as potentially equivalent to shell access under the local user account.

## Limitations

- No forward secrecy. A leaked PSK can decrypt old captured traffic.
- No per-device roles or read-only peers.
- No server-side revocation list.
- Broker/network observers can still see topic names, timing, message sizes, and connection patterns.
- Local OS compromise is out of scope; hcom trusts the local user account and `~/.hcom/config.toml`.

## Incident Response

If a relay token or PSK may have leaked:

```bash
hcom relay off --all
```

This is best-effort. A malicious or offline device can ignore the request.

To continue using relay safely:

1. create a new relay group with `hcom relay new`.
2. move trusted devices to the new token.
3. stop using the old relay ID.
4. rotate any broker password if it may also have leaked.
