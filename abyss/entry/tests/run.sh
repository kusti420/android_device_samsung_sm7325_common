#!/usr/bin/env bash
# Host test of the module installer core: tests/run.sh ARTIFACT.abyssapp [EXPECTED_FINGERPRINT]
# Compiles NixParser from frameworks/base (pure Java) + the installer classes with the host JDK.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
top="$(cd "$here/../../../../../.." && pwd)"
out="${TMPDIR:-/tmp}/abyss-module-test.$$"
mkdir -p "$out"
trap 'rm -rf "$out"' EXIT
src="$here/../src/org/ichthys/abyss"
javac -d "$out" -Xlint:all,-serial \
  "$top/frameworks/base/core/java/com/android/internal/ichthys/config/NixParser.java" \
  "$top/frameworks/base/core/java/com/android/internal/ichthys/config/NixParseException.java" \
  "$src/ModJson.java" "$src/SshSig.java" "$src/ModulePackage.java" "$src/ModuleSpec.java" \
  "$src/ModuleStore.java" "$here/ModuleInstallTest.java"
java -cp "$out" org.ichthys.abyss.ModuleInstallTest "$@"
