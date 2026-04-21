#!/usr/bin/env bash
# deploy-and-wait.sh
#
# Deploys a WAR, waits for the app to be running, then prints session counts.
# Useful as a CI/CD deployment step.
#
# Usage:
#   ./deploy-and-wait.sh /myapp ./target/myapp.war [timeout_seconds]

set -euo pipefail

APP_PATH="${1:?Usage: $0 <context-path> <war-file> [timeout]}"
WAR_FILE="${2:?Usage: $0 <context-path> <war-file> [timeout]}"
TIMEOUT="${3:-120}"
TOMCAT="${TOMCAT_CMD:-tomcat}"

echo "Deploying $WAR_FILE to $APP_PATH…" >&2
"$TOMCAT" deploy --path "$APP_PATH" --file "$WAR_FILE" --update

echo "Waiting for $APP_PATH to be running…" >&2
STATUS=$("$TOMCAT" wait "$APP_PATH" --timeout "$TIMEOUT")

if [[ "$STATUS" != "running" ]]; then
  echo "ERROR: app did not reach 'running' state" >&2
  exit 1
fi

echo "Deployment successful." >&2
echo "Current session counts:" >&2
"$TOMCAT" sessions "$APP_PATH" --output tsv