# Security Policy

## Supported versions

Security fixes are made against the latest release of hcom. Users should upgrade to the newest version when a security fix is published.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's Security tab using Report a vulnerability.

## Relay security

- Relay payloads are end-to-end encrypted. Brokers do not see data.
- Treat the join token like an SSH key or API key.
- If the token may have leaked, run `hcom relay off --all` to disconnect all devices.
- Use a private/custom/self-hosted broker with `--broker` and `--password` for better security.

### Trust model

`hcom relay` is one trust domain for one operator's devices. Membership is all-or-nothing. There are no scoped roles, read-only peers, or per-device permissions.

Relay payloads use a shared PSK with XChaCha20-Poly1305. The encryption binds each payload to the relay, topic, and timestamp. A replay guard drops duplicate envelopes inside a freshness window.

Brokers and network observers cannot read or forge payloads without the PSK. They can still see metadata: topic names, timing, message sizes, and connection patterns.

### What the token means

The join token contains the relay ID, broker URL, and raw PSK. hcom does not ask a server to validate it. It has no expiry, no scope, and no revocation list.

On public brokers, a leaked token gives an attacker full control of the relay. They can decrypt captured traffic, publish authenticated relay traffic, send text to listening agents, launch agents on enrolled devices, kill running agents, and use remote relay RPCs. If those agents can run tools, treat that as shell access on every enrolled device in the relay.

On private brokers with `--password`, the token still leaks the PSK, so captured traffic is still exposed. But the token alone is not enough to publish unless the attacker also has the broker password. Use a private broker when broker-side access control matters, or when the metadata shape of your traffic is itself sensitive. `--password` is broker access control, not another layer of message encryption.

### Limits by design

- Forward secrecy. A leaked PSK can decrypt old captured traffic.
- Per-device attribution inside a relay. Sender identity is routing metadata, not authorization. Every enrolled device speaks with full authority.
- Prompt injection from an authenticated peer. Enrollment is total trust — a peer can launch, kill, and drive agents via RPC, not just send messages. Only enroll devices you would give shell access to.
- Local OS compromise. hcom trusts the local user account and `~/.hcom/config.toml`. It does not defend against another user on the same account or malware with filesystem access.

### Storage

The PSK is stored in `~/.hcom/config.toml`. On Unix, hcom writes that file with mode `0600`.

hcom keeps the PSK out of environment variables. Remote `config_get` and `config_set` refuse `relay_psk`, `relay_token`, `relay_id`, and the broker URL. `hcom relay status` shows only a short fingerprint so two devices can verify they share the same key without printing it.

Anyone who can read that file — another user on the same OS account, malware, or a backup written without preserving permissions — has the full PSK.

### Incident response

Run `hcom relay off --all`. It asks every reachable trusted peer to disable the relay, then disables it locally, so your agents stop acting on attacker messages. It is best-effort damage control, not containment: the attacker's device ignores the request.

The PSK cannot be revoked. There is no server to notify and no denylist to update. Anyone who has the PSK can keep using the old relay until you stop using it.

To keep using relay after a leak, create a new relay with `hcom relay new` and move every trusted device to the new token. Rotation also changes the `relay_id`, so retained state on the old broker topics is orphaned.
