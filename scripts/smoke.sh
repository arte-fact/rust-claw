#!/usr/bin/env bash
# Container smoke test: build the image, boot it, and check the web surface is
# alive and auth-gated. Exits non-zero on any failure. Requires Docker.
set -euo pipefail

IMAGE="claw:smoke"
NAME="claw-smoke-$$"
PORT="${SMOKE_PORT:-8485}"
TOKEN="smoke-token"
BASE="http://127.0.0.1:${PORT}"

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> building image"
docker build -t "$IMAGE" .

echo "==> starting container"
docker run -d --name "$NAME" -p "${PORT}:8080" -e CLAW_AUTH_TOKEN="$TOKEN" "$IMAGE" >/dev/null

echo "==> waiting for /healthz"
for i in $(seq 1 60); do
  if [ "$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/healthz" || true)" = "200" ]; then
    break
  fi
  if [ "$i" = "60" ]; then
    echo "FAIL: healthz never came up"; docker logs "$NAME" || true; exit 1
  fi
  sleep 1
done
echo "ok: healthz"

echo "==> protected route rejects anonymous access"
code="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/chats")"
[ "$code" = "401" ] || { echo "FAIL: expected 401, got $code"; exit 1; }
echo "ok: 401 without auth"

echo "==> login issues a session cookie"
login_code="$(curl -s -o /dev/null -w '%{http_code}' -c /tmp/${NAME}.jar \
  --data "token=${TOKEN}" "${BASE}/login")"
[ "$login_code" = "303" ] || { echo "FAIL: login expected 303, got $login_code"; exit 1; }
echo "ok: login redirects"

echo "==> authenticated API works"
api_code="$(curl -s -o /dev/null -w '%{http_code}' -b /tmp/${NAME}.jar "${BASE}/api/chats")"
[ "$api_code" = "200" ] || { echo "FAIL: authed /api/chats expected 200, got $api_code"; exit 1; }
echo "ok: authed API"

rm -f "/tmp/${NAME}.jar"
echo "==> SMOKE PASSED"
