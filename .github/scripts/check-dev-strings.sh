#!/usr/bin/env bash
# The release build contains nothing of the dev build (docs/ARCHITECTURE.md,
# goal 4). Checked on the binaries, not the source: a stray cfg or an
# optional dependency switched on by feature unification shows up here.
#
#   check-dev-strings.sh RELEASE_DIR DEV_DIR
#
# RELEASE_DIR and DEV_DIR hold mhw_dlss.dll and mhw-dlss-launcher.exe of a
# release and a dev build. Fails if any marker below is in a release binary,
# or if the dev DLL lacks the canary MHW_DLSS_DEV_BUILD (without it this
# check would prove nothing: the dev code would not be in the dev build
# either, or would have been renamed).
set -euo pipefail

release_dir="${1:?release dir}"
dev_dir="${2:?dev dir}"

# The canary: mhw-dev keeps this literal in every dev build.
canary='MHW_DLSS_DEV_BUILD'
# Strings only dev code has: protocol methods and error codes
# (docs/DEV-PROTOCOL.md), dev-only settings keys and defaults
# (docs/ARCHITECTURE.md [Dev]/[Experimental]). Distinctive enough not to
# occur in the Windows runtime, the standard library or NVIDIA's names.
markers=(
  "$canary"
  'probe.jitter' 'probe.reset' 'probe.hdr' 'probe.mv' 'probe.depth'
  'probe.wait' 'probe.modules'
  'mem.read' 'mem.write' 'log.subscribe' 'log.tail' 'settings.set'
  'experiment.set' 'unknown_method' 'bad_request'
  'TokenFile' 'dev.token' 'CaptureDir' 'JitterPhases' 'HoldAntiAliasing'
  '127.0.0.1:47800'
)

status=0
for file in "$release_dir/mhw_dlss.dll" "$release_dir/mhw-dlss-launcher.exe"; do
  [ -f "$file" ] || { echo "::error::$file does not exist"; exit 1; }
  for marker in "${markers[@]}"; do
    if grep -qaF -- "$marker" "$file"; then
      echo "::error file=$file::release binary contains the dev string '$marker'"
      status=1
    fi
  done
done

dev_dll="$dev_dir/mhw_dlss.dll"
[ -f "$dev_dll" ] || { echo "::error::$dev_dll does not exist"; exit 1; }
if ! grep -qaF -- "$canary" "$dev_dll"; then
  echo "::error file=$dev_dll::dev DLL lacks the canary '$canary': the check above proves nothing"
  status=1
fi

if [ "$status" -eq 0 ]; then
  echo "release binaries contain none of ${#markers[@]} dev strings; dev DLL carries $canary"
fi
exit "$status"
