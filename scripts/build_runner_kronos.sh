#!/usr/bin/env bash
# Builds the OPTIONAL Kronos runner (cmsq-runner-kronos). Heavy (~1 GB, torch CPU);
# CI never builds it. Network is used only here, at build time; containers run
# with NetworkMode=none like the base runner.
#  1. builds/ensures the base runner (scripts/build_runner.sh)
#  2. fetches Kronos code at the pinned commit + HF weights at pinned revisions
#     (runner/kronos/kronos.lock), verifies every file against SHA256SUMS
#  3. builds the image, labels it with the manifest hash, records its id in
#     ${CMSQ_SECRETS_DIR:-~/.config/cmsq}/runner_kronos_image_id (gate pins to it)
# CMSQ_KRONOS_CACHE=<dir> reuses already-downloaded weights (still verified).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/runner/kronos"
docker image inspect cmsq-runner:2.2.2 >/dev/null 2>&1 || "$root/scripts/build_runner.sh"
ctx=$(mktemp -d); trap 'rm -rf "$ctx"' EXIT
cp "$src"/{Dockerfile,requirements.lock,SHA256SUMS,kronos.lock,cmsq_kronos.py,gen_synthetic.py} "$ctx/"
cp "$root/runner/signal_runner.py" "$ctx/"  # same runner + deterministic-torch hook
commit=$(awk '$1=="code_commit"{print $2}' "$src/kronos.lock")
git init -q "$ctx/repo" && git -C "$ctx/repo" fetch -q --depth 1 https://github.com/shiyu-coder/Kronos "$commit"
git -C "$ctx/repo" checkout -q FETCH_HEAD
mkdir -p "$ctx/kronos_code/model" "$ctx/weights"
cp "$ctx/repo/LICENSE" "$ctx/kronos_code/" && cp "$ctx/repo/model/"{__init__,kronos,module}.py "$ctx/kronos_code/model/"
rm -rf "$ctx/repo"
awk '$1=="hf"{print $2, $3}' "$src/kronos.lock" | while read -r repo rev; do
  name=${repo#*/}; mkdir -p "$ctx/weights/$name"
  for f in config.json model.safetensors; do
    if [ -n "${CMSQ_KRONOS_CACHE:-}" ] && [ -f "$CMSQ_KRONOS_CACHE/$name/$f" ]; then
      cp "$CMSQ_KRONOS_CACHE/$name/$f" "$ctx/weights/$name/$f"
    else
      curl -sfL -o "$ctx/weights/$name/$f" "https://huggingface.co/$repo/resolve/$rev/$f"
    fi
  done
done
(cd "$ctx" && sha256sum -c --quiet SHA256SUMS)
manifest=$(sha256sum "$src/SHA256SUMS" "$src/kronos.lock" "$src/requirements.lock" "$src/cmsq_kronos.py" "$src/gen_synthetic.py" "$root/runner/signal_runner.py" | sha256sum | cut -d' ' -f1)
docker build --build-arg MANIFEST_SHA256="$manifest" -t cmsq-runner-kronos:2.2.2 "$ctx"
id=$(docker image inspect cmsq-runner-kronos:2.2.2 --format '{{.Id}}')
dir=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
mkdir -p "$dir" && chmod 700 "$dir"
echo "$id" > "$dir/runner_kronos_image_id"
echo "$id manifest=$manifest -> $dir/runner_kronos_image_id"
