# real/

Integration tests that require live environment (installed tools, network, tmux). Not collected by pytest — run directly.

```
python test/public/real/test_pty_delivery.py [claude|gemini|codex|opencode|all]
python test/public/real/test_relay_roundtrip.py
python test/public/real/test_transcript.py
```

- **test_pty_delivery.py** — launches tool in tmux, tests PTY message delivery + gate blocking
- **test_relay_roundtrip.py** — two hcom instances talking through a real MQTT broker
- **test_transcript.py** — parses real transcript files for all tools (claude/gemini/codex/opencode)

Logs written to `logs/`, latest symlinked as `test_pty_delivery_<tool>.latest.log`.
