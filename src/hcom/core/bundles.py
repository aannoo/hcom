"""Bundle helpers for creating and validating bundle events."""

from __future__ import annotations

from typing import Any
import secrets

from .db import log_event
from ..shared import HcomError


def generate_bundle_id() -> str:
    """Generate a short random bundle id."""
    return f"bundle:{secrets.token_hex(4)}"


def get_bundle_quality_hints(bundle: dict[str, Any]) -> list[str]:
    """Return hints when bundle refs are empty.

    Called after validation to warn about missing context.
    Note: All refs fields (transcript, events, files) are now required.
    """
    # All refs are now required, so no quality hints needed
    return []


def validate_bundle(bundle: dict[str, Any]) -> list[str]:
    """Validate bundle payload fields and types.

    Returns list of quality hints (empty refs warnings).
    Raises ValueError for hard validation errors.
    """
    if not isinstance(bundle, dict):
        raise ValueError("bundle must be a JSON object")

    missing = [k for k in ("title", "description", "refs") if k not in bundle]
    if missing:
        raise ValueError(f"Missing required fields: {', '.join(missing)}")

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
        raise ValueError("refs.transcript is required - use 'hcom transcript' to find and include your transcript ranges for context")
    if not refs.get("events"):
        raise ValueError("refs.events is required - use 'hcom events' to find and include relevant event IDs/ranges")
    if not refs.get("files"):
        raise ValueError("refs.files is required - include files you modified/discussed or are related")

    for rng in refs.get("transcript", []):
        if not isinstance(rng, str):
            raise ValueError("refs.transcript items must be strings")

    if "extends" in bundle and not isinstance(bundle.get("extends"), str):
        raise ValueError("extends must be a string")

    if "bundle_id" in bundle and not isinstance(bundle.get("bundle_id"), str):
        raise ValueError("bundle_id must be a string")

    return get_bundle_quality_hints(bundle)


def create_bundle_event(
    bundle: dict[str, Any], *, instance: str, created_by: str | None
) -> str:
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


__all__ = ["generate_bundle_id", "validate_bundle", "create_bundle_event", "get_bundle_quality_hints"]
