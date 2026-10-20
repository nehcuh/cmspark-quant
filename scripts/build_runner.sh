#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../runner"
docker build -t cmsq-runner:1.0.0 .
docker image inspect cmsq-runner:1.0.0 --format '{{.Id}}'
