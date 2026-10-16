#!/usr/bin/env bash
# Exercise the actual image over stdio with the same restrictions documented for users.
#   packaging/container-smoke.sh <local image>
set -euo pipefail
image=$1
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

flags=(--rm --init --read-only --cap-drop=ALL --security-opt=no-new-privileges
       --network=none --memory=2g --memory-swap=2g --pids-limit=256
       --tmpfs=/tmp:rw,nosuid,nodev,size=1g)
want=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n 1)
test "$(docker image inspect --format '{{.Config.User}}' "$image")" = '65532:65532'
test "$(docker run "${flags[@]}" "$image" --version | head -n 1)" = "benchy-mcp $want"

# The default seccomp profile must allow Landlock. Never silently opt out of the OS sandbox.
docker run "${flags[@]}" "$image" doctor --probe --json > "$work/doctor.json"
cat "$work/doctor.json"

# The wrapper lets the existing real MCP client drive Docker just as it drives a binary.
# A read-only bind mount also proves that the image does not need to write into the data folder.
export BENCHY_SMOKE_IMAGE="$image" BENCHY_SMOKE_DATA="$repo/examples/data"
export BENCHY_SMOKE_ARROW="$repo/examples/connectors/hello.arrows"
cat > "$work/benchy" <<'SH'
#!/usr/bin/env bash
exec docker run --rm --init -i --read-only --cap-drop=ALL \
  --security-opt=no-new-privileges --network=none \
  --memory=2g --memory-swap=2g --pids-limit=256 \
  --tmpfs=/tmp:rw,nosuid,nodev,size=1g \
  --mount "type=bind,src=$BENCHY_SMOKE_DATA,dst=/data,readonly" \
  --mount "type=bind,src=$BENCHY_SMOKE_ARROW,dst=/hello.arrows,readonly" \
  "$BENCHY_SMOKE_IMAGE" "$@"
SH
chmod +x "$work/benchy"
python3 "$repo/examples/verify.py" "$work/benchy" /data
# Docker transports bytes over stdin; Unix descriptor passing and inherited stream-channel
# descriptors do not cross its CLI. Exercise the host protocol's mounted-file path here.
python3 - "$work/benchy" <<'PY'
import json, struct, subprocess, sys
requests = [
    {"id": 1, "call": "hello", "protocol": [6]},
    {"id": 2, "call": "attach", "name": "readings", "path": "/hello.arrows"},
    {"id": 3, "call": "query", "sql": "SELECT count(*) AS n FROM readings"},
    {"id": 4, "call": "describe"},
]
frames = bytearray()
for request in requests:
    body = json.dumps(request).encode()
    frames += struct.pack(">I", len(body)) + body
result = subprocess.run([sys.argv[1], "host"], input=frames, capture_output=True, timeout=60)
assert result.returncode == 0, result.stderr.decode(errors="replace")
replies, offset = {}, 0
while offset < len(result.stdout):
    (size,) = struct.unpack_from(">I", result.stdout, offset)
    offset += 4
    reply = json.loads(result.stdout[offset:offset + size])
    offset += size
    assert reply["id"] not in replies, reply
    replies[reply["id"]] = reply
assert set(replies) == {1, 2, 3, 4}, replies
assert replies[1]["protocol"] == 6, replies[1]
assert replies[2]["ok"] and replies[2]["rows"] > 0, replies[2]
assert replies[3]["ok"] and replies[3]["results"][0]["rows"] == 1, replies[3]
assert str(replies[2]["rows"]) in replies[3]["text"], replies[3]
assert replies[4]["ok"] and "readings" in replies[4]["text"], replies[4]
print("container host protocol: hello, attach, query, describe and clean EOF passed")
PY

# The image must carry the notices distributed in the native bundles.
docker run "${flags[@]}" --entrypoint /bin/sh "$image" -ec \
  'test -s /opt/benchy/LICENSE && test -s /opt/benchy/LICENSE-DuckDB && test -s /opt/benchy/THIRD-PARTY-LICENSES.txt'
echo "container smoke passed: $image"
