#!/usr/bin/env bash
# Builds the pinned signal-runner image. Needs access to the docker daemon
# (see README "Docker setup"). Prints the image id that jobs are pinned to.
set -euo pipefail
cd "$(dirname "$0")/../runner"
docker build -t cmsq-runner:2.0.0 .
docker image inspect cmsq-runner:2.0.0 --format '{{.Id}} {{index .Config.Labels "cmsq.runner.version"}}'
