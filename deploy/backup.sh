#!/bin/bash
# Nightly SQLite backup. Uses sqlite3's `.backup` command rather than a
# plain `cp` -- a raw copy of a WAL-mode database file while the
# server is writing to it can capture an inconsistent snapshot;
# `.backup` uses SQLite's own backup API and is safe to run live.
set -euo pipefail

SRC=/opt/reminder/reminder.db
DEST_DIR=/opt/reminder/backups
KEEP_DAYS=14

mkdir -p "$DEST_DIR"
sqlite3 "$SRC" ".backup '$DEST_DIR/reminder-$(date +%F).db'"
find "$DEST_DIR" -name 'reminder-*.db' -mtime "+$KEEP_DAYS" -delete
