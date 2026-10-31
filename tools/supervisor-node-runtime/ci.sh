#!/usr/bin/env bash
# Owned GitHub Linux candidate build only. No service, installation or host repair.
set -euo pipefail
test "${GITHUB_ACTIONS:-}" = true
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
recipe="$(cd -- "$(dirname -- "$0")" && pwd)"
test -n "${RUNNER_TEMP:-}"
test -n "${GITHUB_ENV:-}"
test -n "${GITHUB_OUTPUT:-}"
output="$RUNNER_TEMP/noevia-supervisor-node-runtime"
if [ ! -e "$output" ]; then
  owned="$(mktemp -d "$RUNNER_TEMP/noevia-node-build.XXXXXXXX")"
  name="noevia-node-build-$(basename "$owned")"
  printf 'diagnostic=%s/output\n' "$owned" >> "$GITHUB_OUTPUT"
  # Only this uniquely named ephemeral container is cleaned on exit/deadline.
  trap 'docker rm --force "$name" >/dev/null 2>&1 || true' EXIT
  python3 "$recipe/runtime.py" download "$owned/source.tar.xz"
  builder="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["builder"]["linuxAmd64Manifest"])' "$recipe/source-lock.json")"
  image="node@$builder"
  docker pull --platform linux/amd64 "$image"
  docker image inspect "$image" > "$owned/builder-inspect.json"
  mkdir "$owned/home" "$owned/tmp"
  timeout --kill-after=15s 165m docker run --rm --name "$name" --pull never \
    --platform linux/amd64 --user "$(id -u):$(id -g)" --network none \
    --read-only --cap-drop ALL --security-opt no-new-privileges:true \
    --pids-limit 256 --memory 6g --memory-swap 6g --cpus 2 \
    --mount "type=bind,src=$recipe,dst=/recipe,readonly" \
    --mount "type=bind,src=$owned,dst=/work" \
    --entrypoint /usr/bin/env "$image" -i \
    PATH=/usr/local/bin:/usr/bin:/bin HOME=/work/home TMPDIR=/work/tmp \
    LC_ALL=C.UTF-8 TZ=UTC CC=/usr/bin/gcc CXX=/usr/bin/g++ NOEVIA_NODE_BUILDER="$builder" \
    python3 /recipe/runtime.py build /work/source.tar.xz /work/source-work /work/output
  python3 "$recipe/runtime.py" verify "$owned/output"
  mv "$owned/output" "$output"
fi
if [ -d "$output" ]; then
  printf 'diagnostic=%s\n' "$output" >> "$GITHUB_OUTPUT"
fi
# A corrupt or incomplete exact-key cache fails; it is never a silent rebuild
# or fallback to the hosted Node. This scoped cache is not a release artifact.
python3 "$recipe/runtime.py" verify "$output"
printf 'NOEVIA_SUPERVISOR_TEST_NODE=%s/node\n' "$output" >> "$GITHUB_ENV"
