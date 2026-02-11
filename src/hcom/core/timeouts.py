#!/usr/bin/env python3
"""Timeout constants for HCOM subsystems.

Centralizes all timing thresholds for:
- Process registration
- Heartbeat detection
- Status timeouts
- Instance cleanup

Rationale for key values are documented inline.
"""

from __future__ import annotations

# ==================== Process Registration ====================
# How long to wait for process binding to be created after launch
PROCESS_REGISTRATION_TIMEOUT = 30  # seconds

# Max time between instance creation and session binding before considering launch failed
LAUNCH_PLACEHOLDER_TIMEOUT = 30  # seconds


# ==================== Heartbeat & Liveness ====================
# Heartbeat timeout with active TCP listener (PTY, hooks with notify)
# 35s = 30s hook polling interval + 5s buffer for processing/network
HEARTBEAT_THRESHOLD_TCP = 35  # seconds

# Heartbeat timeout without TCP listener (adhoc instances)
# Shorter timeout for faster dead instance detection
HEARTBEAT_THRESHOLD_NO_TCP = 10  # seconds

# Heartbeat age to use when last_stop is missing (marker for unreliable data)
UNKNOWN_HEARTBEAT_AGE = 999999  # seconds (effectively infinite)


# ==================== Status Activity ====================
# Max time without status update before marking inactive
# Generous timeout for long-running operations (complex tool execution)
STATUS_ACTIVITY_TIMEOUT = 300  # seconds (5 minutes)


# ==================== Cleanup Thresholds ====================
# How long placeholder instances (launching, no session) can exist before cleanup
CLEANUP_PLACEHOLDER_THRESHOLD = 120  # seconds (2 minutes)

# How long stale instances (stale:* context) can exist before cleanup
CLEANUP_STALE_THRESHOLD = 3600  # seconds (1 hour)

# How long any inactive instances can exist before cleanup
CLEANUP_INACTIVE_THRESHOLD = 43200  # seconds (12 hours)

# Grace period after system sleep/wake before resuming stale cleanup
# Allows delivery threads to write fresh heartbeats after wake
WAKE_GRACE_PERIOD = 60  # seconds (2x heartbeat interval)


# ==================== Delivery & PTY ====================
# User activity cooldown: don't inject messages if user typed within this window
USER_ACTIVITY_COOLDOWN = 0.5  # seconds

# Initial retry delay for failed delivery attempts
INITIAL_RETRY_DELAY = 0.25  # seconds

# Output stability requirement before injection (DeliveryGate)
OUTPUT_STABLE_SECONDS = 1.0  # seconds

# Cursor verification timeout (wait for cursor to advance after injection)
VERIFY_CURSOR_TIMEOUT = 10.0  # seconds

# Slow systems may take longer for PTY ready pattern detection
PTY_VERIFY_TIMEOUT = 10.0  # seconds


# ==================== Subscriptions & Polling ====================
# How long to keep "stopped" instances visible for reclaim/relaunch
RECENTLY_STOPPED_MINUTES = 10  # minutes


__all__ = [
    # Process registration
    "PROCESS_REGISTRATION_TIMEOUT",
    "LAUNCH_PLACEHOLDER_TIMEOUT",
    # Heartbeat
    "HEARTBEAT_THRESHOLD_TCP",
    "HEARTBEAT_THRESHOLD_NO_TCP",
    "UNKNOWN_HEARTBEAT_AGE",
    # Status
    "STATUS_ACTIVITY_TIMEOUT",
    # Cleanup
    "CLEANUP_PLACEHOLDER_THRESHOLD",
    "CLEANUP_STALE_THRESHOLD",
    "CLEANUP_INACTIVE_THRESHOLD",
    "WAKE_GRACE_PERIOD",
    # Delivery
    "USER_ACTIVITY_COOLDOWN",
    "INITIAL_RETRY_DELAY",
    "OUTPUT_STABLE_SECONDS",
    "VERIFY_CURSOR_TIMEOUT",
    "PTY_VERIFY_TIMEOUT",
    # Polling
    "RECENTLY_STOPPED_MINUTES",
]
