#!/usr/bin/env bash
# Host test of the Wi-Fi hand-over's security merge: tests/run_wifi.sh (no arguments).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="${TMPDIR:-/tmp}/abyss-wifi-test.$$"
mkdir -p "$out"
trap 'rm -rf "$out"' EXIT
src="$here/../src/org/ichthys/abyss"
javac -d "$out" -Xlint:all "$src/WifiSecurity.java" "$here/WifiSecurityTest.java"
java -cp "$out" org.ichthys.abyss.WifiSecurityTest
