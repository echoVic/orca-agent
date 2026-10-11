#!/usr/bin/env bash
# Remove what earlier Harbor runs left behind in Docker (issue #129).
#
# Usage: scripts/eval/reap_harbor.sh
#
# Harbor runs each trial's environment as a Compose project named
# `<task>__<trial id>__env` (a separate verifier: `…__verifier__<key>`). An
# environment-start timeout abandons its project: the containers go on building
# or downloading while later trials start, and removing them by hand leaves
# their networks behind. Run this before and after every round, never while a
# Harbor run is in progress, and note a non-zero count in the report.
#
# A name is not proof of ownership, so a project with containers is reaped only
# when they were started from Harbor's own compose files. A project with no
# container left is matched by Harbor's name shape, and all that remains of it
# is an unused network.
set -uo pipefail

if ! docker info >/dev/null 2>&1; then
  echo "reap_harbor: Docker is not running" >&2
  exit 2
fi

shape='^[a-z0-9][a-z0-9_-]*__[a-z0-9]{7}__(env|verifier__[a-z0-9_-]+)$'
containers_by_project=$(
  docker ps -a --format '{{.Label "com.docker.compose.project"}}|{{.Label "com.docker.compose.project.config_files"}}'
)
with_containers=$(cut -d'|' -f1 <<<"$containers_by_project" | grep -E "$shape" | sort -u)
started_by_harbor=$(
  grep -E '\|.*harbor.*docker-compose-(build|prebuilt)\.yaml' <<<"$containers_by_project" \
    | cut -d'|' -f1 | grep -E "$shape" | sort -u
)
network_only=$(
  docker network ls --format '{{.Label "com.docker.compose.project"}}' | grep -E "$shape" | sort -u \
    | comm -23 - <(printf '%s\n' "$with_containers")
)

for project in $(comm -23 <(printf '%s\n' "$with_containers") <(printf '%s\n' "$started_by_harbor")); do
  echo "reap_harbor: skipped $project: its containers were not started by Harbor" >&2
done

status=0
reaped=0
containers=0
for project in $(printf '%s\n' "$started_by_harbor" "$network_only" | sort -u); do
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
