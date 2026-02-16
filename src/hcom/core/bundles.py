"""Bundle helpers for creating and validating bundle events."""

from __future__ import annotations

from typing import Any
import secrets

from .db import log_event
from .detail_levels import validate_detail_level
from ..shared import HcomError, SenderIdentity


def parse_csv_list(raw: str | None) -> list[str]:
    """Parse comma-separated list into list of strings.

    Args:
        raw: Comma-separated string (e.g., "a,b,c") or None

    Returns:
        List of stripped, non-empty strings
    """
    if raw is None:
        return []
    parts = [p.strip() for p in raw.split(",")]
    return [p for p in parts if p]


def get_bundle_instance_name(identity: SenderIdentity) -> str:
    """Get bundle instance name from identity.

    Args:
        identity: Sender identity

    Returns:
        Instance name for bundle event
    """
    if identity.kind == "external":
        return f"ext_{identity.name}"
    elif identity.kind == "system":
        return f"sys_{identity.name}"
    else:
        return identity.name


def parse_inline_bundle_flags(argv: list[str]) -> tuple[dict | None, list[str]]:
    """Parse inline bundle creation flags from argv.

    Parses: --title, --description, --events, --files, --transcript, --extends

    Args:
        argv: Command line arguments

    Returns:
        (bundle_dict, remaining_argv): Bundle dict if --title present, None otherwise.
        remaining_argv has bundle flags removed.

    Raises:
        ValueError: If required fields missing or invalid format
    """
    from ..commands.utils import parse_flag_value

    # Check if any bundle flags are present
    bundle_flags = {"--title", "--description", "--events", "--files", "--transcript", "--extends"}
    has_bundle_flags = any(flag in argv for flag in bundle_flags)

    # Check for duplicate flags
    for flag in bundle_flags:
        count = argv.count(flag)
        if count > 1:
            raise ValueError(f"Duplicate flag {flag} (found {count} times)")

    # If bundle flags present but no --title, error
    if has_bundle_flags and "--title" not in argv:
        present = [f for f in bundle_flags if f in argv]
        raise ValueError(f"Bundle flags require --title: found {', '.join(present)} without --title")

    # Check if --title is present (required to trigger inline bundle creation)
    if "--title" not in argv:
        return None, argv

    # Parse all bundle flags
    # Once --title is present, we require proper values for flags if they're used
    from ..commands.utils import CLIError

    try:
        title_val, argv = parse_flag_value(argv, "--title", required=True)
        if not title_val:
            raise ValueError("--title is required for inline bundle creation")

        description_val, argv = parse_flag_value(argv, "--description", required=True)
        if not description_val:
            raise ValueError("--description is required when --title is present")

        # For optional flags, use required=True so we get proper "flag requires value" errors
        # if the flag is present but has no value (rather than silent None)
        events_val, argv = parse_flag_value(argv, "--events", required=True)
        files_val, argv = parse_flag_value(argv, "--files", required=True)
        transcript_val, argv = parse_flag_value(argv, "--transcript", required=True)
        extends_val, argv = parse_flag_value(argv, "--extends", required=True)

    except CLIError as e:
        # Convert CLIError to ValueError for consistent error handling
        raise ValueError(str(e))
    except ValueError as e:
        raise ValueError(f"Error parsing bundle flags: {e}")

    # Parse CSV lists
    events = parse_csv_list(events_val) if events_val else []
    files = parse_csv_list(files_val) if files_val else []
    transcript = parse_csv_list(transcript_val) if transcript_val else []

    # Construct bundle dict
    bundle = {
        "title": title_val,
        "description": description_val,
        "refs": {"events": events, "files": files, "transcript": transcript},
    }

    if extends_val:
        bundle["extends"] = extends_val

    return bundle, argv


def generate_bundle_id() -> str:
    """Generate a short random bundle id."""
    return f"bundle:{secrets.token_hex(4)}"


def parse_transcript_ref(ref: str | dict[str, Any]) -> dict[str, Any]:
    """Parse a transcript reference into normalized format.

    Accepts:
    - String with colon notation: "3-14:normal", "6:full"
    - Object: {"range": "6", "detail": "full", "note": "design spec"}

    Returns: {"range": "6", "detail": "full", "note": "..."}
    """
    if isinstance(ref, dict):
        if "range" not in ref:
            raise ValueError("Transcript ref object must have 'range' field")
        if "detail" not in ref:
            raise ValueError("Transcript ref object must have 'detail' field")
        detail = ref["detail"]
        validate_detail_level(detail)
        return ref

    if isinstance(ref, str):
        if ":" not in ref:
            raise ValueError(
                f'Transcript ref must include detail level. Got: \'{ref}\'\nFormat: "range:detail" (e.g., "3-14:normal", "10:full", "20-25:detailed")'
            )

        range_part, detail = ref.split(":", 1)
        if not range_part.strip():
            raise ValueError(f"Empty range in transcript ref: '{ref}'")
        if not detail.strip():
            raise ValueError(f"Empty detail level in transcript ref: '{ref}'")

        detail = detail.strip()
        validate_detail_level(detail)
        return {"range": range_part.strip(), "detail": detail}

    raise ValueError(f"Transcript ref must be a string or object, got {type(ref)}")


def validate_bundle(bundle: dict[str, Any]) -> None:
    """Validate bundle payload fields and types.

    Raises ValueError for hard validation errors.
    """
    if not isinstance(bundle, dict):
        raise ValueError("bundle must be a JSON object")

    missing = [k for k in ("title", "description", "refs") if k not in bundle]
    if missing:
        raise ValueError(f"Missing required fields: {', '.join(missing)}")

    # Estimate bundle cat output size to prevent massive outputs
    refs = bundle.get("refs", {})
    if isinstance(refs, dict):
        files = refs.get("files", [])
        events = refs.get("events", [])
        transcript = refs.get("transcript", [])

        # Conservative estimates for bundle cat output
        # Files: just metadata (~1 line each)
        # Events: ~50 lines JSON each (can be large)
        # Transcript: ~500 lines each (truncated unless --complete)
        estimated_lines = (
            len(files) * 1  # file metadata
            + len(events) * 50  # event JSON
            + len(transcript) * 500  # transcript entries
        )

        MAX_ESTIMATED_LINES = 15000  # ~15K lines is reasonable upper bound
        if estimated_lines > MAX_ESTIMATED_LINES:
            raise ValueError(
                f"Bundle too large (estimated {estimated_lines:,} lines of output). "
                f"Limit is {MAX_ESTIMATED_LINES:,} lines. Split into multiple smaller bundles."
            )

    if not isinstance(bundle.get("title"), str):
        raise ValueError("title must be a string")
    if not isinstance(bundle.get("description"), str):
        raise ValueError("description must be a string")

    refs = bundle.get("refs")
    if not isinstance(refs, dict):
        raise ValueError("refs must be an object")

    for key in ("events", "files", "transcript"):
        if key not in refs:
            raise ValueError(f"refs.{key} is required")

    if not isinstance(refs.get("events"), list):
        raise ValueError("refs.events must be a list")
    if not isinstance(refs.get("files"), list):
        raise ValueError("refs.files must be a list")
    if not isinstance(refs.get("transcript"), list):
        raise ValueError("refs.transcript must be a list")

    # Require non-empty refs to prevent lazy handoffs
    if not refs.get("transcript"):
        raise ValueError(
            'refs.transcript is required\nFind ranges: hcom transcript <agent> [--last N]\nFormat: "1-5:normal,10:full"'
        )
    if not refs.get("events"):
        raise ValueError('refs.events is required\nFind events: hcom events [--last N]\nFormat: "123,124" or "100-105"')
    if not refs.get("files"):
        raise ValueError(
            'refs.files is required\nInclude files you created, modified, discussed, or are relevant\nFormat: "src/main.py,tests/test.py"'
        )

    # Parse and normalize transcript refs
    normalized_transcript = []
    for ref in refs.get("transcript", []):
        try:
            parsed = parse_transcript_ref(ref)
            normalized_transcript.append(parsed)
        except ValueError as e:
            raise ValueError(f"Invalid transcript ref: {e}")

    # Replace with normalized refs
    refs["transcript"] = normalized_transcript

    # Check file existence (warn but don't error - files might be on another device)
    from pathlib import Path

    missing_files = []
    for file_path in refs.get("files", []):
        if not Path(file_path).exists():
            missing_files.append(file_path)

    if missing_files:
        import sys

        print(f"Warning: {len(missing_files)} file(s) not found locally:", file=sys.stderr)
        for f in missing_files[:5]:  # Show first 5
            print(f"  - {f}", file=sys.stderr)
        if len(missing_files) > 5:
            print(f"  ... and {len(missing_files) - 5} more", file=sys.stderr)

    # Validate extends reference
    if "extends" in bundle:
        if not isinstance(bundle.get("extends"), str):
            raise ValueError("extends must be a string")
        extends_val = bundle.get("extends")
        if extends_val:
            # Check if parent bundle exists
            from .db import get_db

            try:
                conn = get_db()
                # Parse bundle ID (might be "bundle:abc123" or just "abc123")
                search_id = extends_val if extends_val.startswith("bundle:") else f"bundle:{extends_val}"
                row = conn.execute(
                    "SELECT id FROM events WHERE type = 'bundle' AND json_extract(data, '$.bundle_id') = ?",
                    (search_id,),
                ).fetchone()
                if not row:
                    raise ValueError(f"Parent bundle not found: {extends_val}")
            except Exception as e:
                # If we can't check, just warn
                import sys

                print(f"Warning: Could not validate parent bundle '{extends_val}': {e}", file=sys.stderr)

    if "bundle_id" in bundle and not isinstance(bundle.get("bundle_id"), str):
        raise ValueError("bundle_id must be a string")


def create_bundle_event(bundle: dict[str, Any], *, instance: str, created_by: str | None) -> str:
    """Create a bundle event and return its bundle_id."""
    try:
        validate_bundle(bundle)
    except ValueError as e:
        raise HcomError(str(e))

    data = dict(bundle)
    bundle_id = data.get("bundle_id") or generate_bundle_id()
    data["bundle_id"] = bundle_id
    if created_by:
        data["created_by"] = created_by

    log_event(event_type="bundle", instance=instance, data=data)
    return bundle_id


__all__ = [
    "generate_bundle_id",
    "validate_bundle",
    "create_bundle_event",
    "parse_csv_list",
    "get_bundle_instance_name",
    "parse_inline_bundle_flags",
]
