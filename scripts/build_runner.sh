#!/usr/bin/env bash
# Builds the pinned signal-runner image. Needs access to the docker daemon
# (see README "Docker setup"). Records the image id that the gate pins jobs to
# (runner_pinned compares the digest, not just the label) in
# ${CMSQ_SECRETS_DIR:-~/.config/cmsq}/runner_image_id — outside any workspace.
set -euo pipefail
cd "$(dirname "$0")/../runner"
docker build -t cmsq-runner:2.1.0 .
id=$(docker image inspect cmsq-runner:2.1.0 --format '{{.Id}}')
dir=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
mkdir -p "$dir" && chmod 700 "$dir"
echo "$id" > "$dir/runner_image_id"
echo "$id $(docker image inspect cmsq-runner:2.1.0 --format '{{index .Config.Labels "cmsq.runner.version"}}') -> $dir/runner_image_id"
