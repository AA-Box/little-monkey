#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ANDROID_STUDIO="${ANDROID_STUDIO:-/Applications/Android Studio.app/Contents}"
JAVA_HOME="${JAVA_HOME:-$ANDROID_STUDIO/jbr/Contents/Home}"
GSON_JAR="${GSON_JAR:-$ANDROID_STUDIO/lib/intellij.libraries.gson.jar}"
OUT="$(mktemp -d "${TMPDIR:-/tmp}/little-monkey-jetbrains-contract.XXXXXX")"
trap 'rm -rf "$OUT"' EXIT

if [[ ! -x "$JAVA_HOME/bin/javac" ]]; then
  echo "Set JAVA_HOME to a JDK 17+ installation." >&2
  exit 2
fi
if [[ ! -f "$GSON_JAR" ]]; then
  echo "Set GSON_JAR to gson 2.x (or install Android Studio)." >&2
  exit 2
fi

"$JAVA_HOME/bin/javac" --release 17 -Xlint:all,-classfile -cp "$GSON_JAR" -d "$OUT" \
  "$ROOT/src/main/java/app/littlemonkey/jetbrains/AcpClient.java" \
  "$ROOT/src/main/java/app/littlemonkey/jetbrains/AcpProtocol.java" \
  "$ROOT/src/main/java/app/littlemonkey/jetbrains/LittleMonkeyLaunchConfig.java" \
  "$ROOT/src/test/java/app/littlemonkey/jetbrains/AcpContractTest.java"

"$JAVA_HOME/bin/java" -ea -cp "$OUT:$GSON_JAR" app.littlemonkey.jetbrains.AcpContractTest
