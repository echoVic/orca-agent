#!/usr/bin/env bash
# Remove what earlier Harbor runs left behind in Docker (issue #129).
#
# Usage: scripts/eval/reap_harbor.sh
#
# Harbor runs each trial's environment as a Compose project named
# `<task>__<trial>__env` (a separate verifier: `<task>__<trial>__verifier__<key>`).
# An environment-start timeout abandons its project: the containers go on
# building or downloading while later trials start, and removing them by hand
# leaves their networks behind. Run this before and after every round, never
# while a Harbor run is in progress, and note a non-zero count in the report.
set -uo pipefail

if ! docker info >/dev/null 2>&1; then
  echo "reap_harbor: Docker is not running" >&2
  exit 2
fi

projects=$(
  {
    docker ps -a --format '{{.Label "com.docker.compose.project"}}'
    docker network ls --format '{{.Label "com.docker.compose.project"}}'
  } | grep -E '__(env|verifier__.+)$' | sort -u
)

status=0
reaped=0
containers=0
for project in $projects; do
  count=$(docker ps -aq --filter "label=com.docker.compose.project=$project" | wc -l | tr -d ' ')
  if docker compose -p "$project" down --remove-orphans --timeout 5 >/dev/null 2>&1; then
    echo "reaped $project ($count containers)"
    reaped=$((reaped + 1))
    containers=$((containers + count))
  else
    echo "reap_harbor: could not remove $project" >&2
    status=1
  fi
done
echo "leaked Harbor projects reaped: $reaped, containers: $containers"
exit "$status"
