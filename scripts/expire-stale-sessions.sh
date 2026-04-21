#!/usr/bin/env bash
# expire-stale-sessions.sh
#
# Lists all running apps, reads their session idle histogram,
# and expires sessions idle >= IDLE_THRESHOLD minutes.
#
# Usage:
#   TOMCAT_URL=http://localhost:8080 \
#   TOMCAT_USER=admin \
#   TOMCAT_PASSWORD=secret \
#   ./expire-stale-sessions.sh [idle_minutes]
#
# Default idle threshold: 60 minutes

set -euo pipefail

IDLE_THRESHOLD="${1:-60}"
TOMCAT="${TOMCAT_CMD:-tomcat}"

echo "=== Expiring sessions idle >= ${IDLE_THRESHOLD} min ===" >&2

# 1. List running apps
#    list --output tsv  →  path TAB status TAB sessions TAB description
RUNNING_APPS=$(
  "$TOMCAT" list --output tsv \
    | awk -F'\t' '$2 == "running" { print $1 }'
)

if [[ -z "$RUNNING_APPS" ]]; then
  echo "No running applications found." >&2
  exit 0
fi

# 2. For each app, read session buckets and pipe stale ones into expire-sessions
#
#    sessions --output tsv  →  idle_minutes TAB session_count TAB app_path
#
#    We filter rows where idle_minutes >= threshold,
#    reshape to "path idle_minutes", feed to expire-sessions --stdin.

echo "$RUNNING_APPS" | while read -r app_path; do
  echo "--- Checking $app_path ---" >&2

  STALE=$(
    "$TOMCAT" sessions "$app_path" --output tsv 2>/dev/null \
      | awk -F'\t' -v thr="$IDLE_THRESHOLD" '$1 >= thr { print $3 "\t" $1 }'
  )

  if [[ -z "$STALE" ]]; then
    echo "  No stale sessions." >&2
    continue
  fi

  # expire-sessions --stdin reads lines of "path<whitespace>idle_minutes"
  echo "$STALE" | "$TOMCAT" expire-sessions --stdin
done

echo "=== Done ===" >&2