#!/usr/bin/env bash
# session-report.sh
#
# Prints a summary of session counts across all running apps.
# Outputs TSV: app_path  total_sessions  max_idle_minutes

set -euo pipefail
TOMCAT="${TOMCAT_CMD:-tomcat}"

echo -e "app_path\ttotal_sessions\tmax_idle_minutes"

"$TOMCAT" list --output tsv \
  | awk -F'\t' '$2 == "running" && $3 > 0 { print $1, $3 }' \
  | while read -r path total; do
      # sessions TSV: idle_minutes TAB count TAB path
      MAX_IDLE=$(
        "$TOMCAT" sessions "$path" --output tsv 2>/dev/null \
          | awk -F'\t' 'BEGIN{m=0} $1>m{m=$1} END{print m}'
      )
      echo -e "${path}\t${total}\t${MAX_IDLE:-0}"
    done