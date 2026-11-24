#!/bin/bash
# healthcheck.js's checks in bash, so the Rust image's health does not depend on a JavaScript runtime.
set -uo pipefail

PORT="${PORT:-3004}"

fetch() {
  exec 3<>"/dev/tcp/127.0.0.1/${PORT}" || return 1
  printf 'GET %s HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n' "$1" >&3
  cat <&3
}

fail() {
  echo "Healthcheck failed: $*"
  exit 1
}

expect_200() {
  local status
  status=$(printf '%s' "$2" | head -n1 | tr -d '\r')
  case "$status" in
    "HTTP/1."?" 200"*) ;;
    *) fail "$1 returned ${status:-nothing}" ;;
  esac
}

health=$(fetch /api/health) || fail "/api/health unreachable"
expect_200 /api/health "$health"
checks=$(printf '%s' "${health#*$'\r\n\r\n'}" | grep -o '"checks":{[^}]*}') || fail "/api/health listed no checks"
checks=${checks#'"checks":{'}
unhealthy=$(printf '%s' "${checks%'}'}" | tr ',' '\n' | grep -v ':"ok"$' | cut -d'"' -f2 | paste -sd, - | sed 's/,/, /g')
[ -z "$unhealthy" ] || fail "Unhealthy services: $unhealthy"

root=$(fetch /) || fail "/ unreachable"
expect_200 / "$root"
printf '%s' "${root%%$'\r\n\r\n'*}" | grep -qi '^content-type:.*text/html' || fail "/ content-type was not text/html"
