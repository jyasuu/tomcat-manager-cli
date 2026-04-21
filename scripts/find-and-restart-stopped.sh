#!/usr/bin/env bash
# find-and-restart-stopped.sh
#
# Finds all stopped apps and restarts them.
# Can be run as a cron job for self-healing.

set -euo pipefail
TOMCAT="${TOMCAT_CMD:-tomcat}"

STOPPED=$(
  "$TOMCAT" list --output tsv \
    | awk -F'\t' '$2 == "stopped" { print $1 }'
)

if [[ -z "$STOPPED" ]]; then
  echo "All apps running." >&2
  exit 0
fi

echo "Restarting stopped apps:" >&2
echo "$STOPPED" | while read -r path; do
  echo "  Starting $path…" >&2
  "$TOMCAT" start "$path"
done