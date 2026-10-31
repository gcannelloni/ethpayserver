#!/usr/bin/env bash
# Proves cosign-sign.sh actually goes red and green on the cases it claims
# to, against a fake `cosign` on PATH rather than a comment asserting someone
# ran it once. In particular: a signing failure must name signing in the
# error, not the digest guard that already passed - that distinction is the
# whole reason this script exists, and nothing in normal CI operation
# exercises cosign failing, so nothing organically proves it.
#
# The fake accepts any arguments, so it cannot tell us whether a flag is
# valid. The replay cases below hand the recorded arguments to the real
# pinned cosign, whose own argument validation then decides.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="$HERE/cosign-sign.sh"
NO_TLOG_CONFIG="$HERE/cosign-signing-config-no-tlog.json"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAKE_DIGEST="ghcr.io/agent8-cash/ethpayserver/evmmonitor@sha256:deadbeef"

fail=0

# The real cosign, resolved before the fake goes on PATH. It must be the
# version CI pins: a different version validates flags differently.
REAL_COSIGN="$(command -v cosign || true)"
if [ -z "$REAL_COSIGN" ] || ! "$REAL_COSIGN" version 2>/dev/null | grep -q 'GitVersion: *v3\.0\.6$'; then
  echo "FAIL: the pinned cosign v3.0.6 is not on PATH, so the argument replay cannot run"; exit 1
fi

# Replays the arguments the last successful sign recorded against the real
# pinned cosign. Cosign reports a bad flag before it parses the reference,
# so reaching "could not parse reference" proves the flags were accepted.
# The reference is deliberately unparseable-by-design: no registry is contacted.
replay_against_pinned() {  # name
  local out args=()
  mapfile -t args < "$COSIGN_CALL_LOG"
  out="$(timeout 60 "$REAL_COSIGN" "${args[@]}" 2>&1 </dev/null)"
  if grep -qiE "not supported|unknown flag|deprecated|Error: --" <<<"$out"; then
    echo "FAIL: pinned cosign rejected the arguments of $1 - args: ${args[*]} out: $out"; fail=1
  elif ! grep -q "could not parse reference" <<<"$out"; then
    echo "FAIL: pinned cosign did not get past argument validation for $1 - out: $out"; fail=1
  else
    echo "ok: pinned cosign accepts the arguments of $1"
  fi
}

# A real cosign that signs successfully. One argument per line.
cat > "$TMP/cosign" <<'EOF2'
#!/usr/bin/env bash
printf '%s\n' "$@" >> "$COSIGN_CALL_LOG"
exit 0
EOF2
chmod +x "$TMP/cosign"

export COSIGN_CALL_LOG="$TMP/calls.log"

# tlog_upload=false signs with the no-transparency-log config, and passes no
# --tlog-upload flag: the pinned cosign refuses that flag with a signing config.
: > "$COSIGN_CALL_LOG"
if out="$(PATH="$TMP:$PATH" "$CHECK" monitor "$FAKE_DIGEST" false 2>&1)"; then
  if grep -qx -- "--signing-config" "$COSIGN_CALL_LOG" \
    && grep -qx -- "$NO_TLOG_CONFIG" "$COSIGN_CALL_LOG" \
    && grep -qx -- "$FAKE_DIGEST" "$COSIGN_CALL_LOG" \
    && ! grep -q -- "--tlog-upload" "$COSIGN_CALL_LOG"; then
    echo "ok: tlog_upload=false signs the digest with the no-tlog signing config and no --tlog-upload"
    replay_against_pinned "tlog_upload=false"
  else
    echo "FAIL: tlog_upload=false did not pass the no-tlog signing config and the digest - got: $(cat "$COSIGN_CALL_LOG")"; fail=1
  fi
else
  echo "FAIL: a successful cosign sign is reported as a failure - $out"; fail=1
fi

# tlog_upload=true (release tags) passes no transparency flag: v3 uploads by default.
: > "$COSIGN_CALL_LOG"
if out="$(PATH="$TMP:$PATH" "$CHECK" server "$FAKE_DIGEST" true 2>&1)"; then
  if grep -qx -- "$FAKE_DIGEST" "$COSIGN_CALL_LOG" && ! grep -q -- "--tlog-upload\|--signing-config" "$COSIGN_CALL_LOG"; then
    echo "ok: tlog_upload=true signs the digest with no transparency flag or signing config"
    replay_against_pinned "tlog_upload=true"
  else
    echo "FAIL: tlog_upload=true did not sign the digest with no transparency flag - got: $(cat "$COSIGN_CALL_LOG")"; fail=1
  fi
else
  echo "FAIL: a successful cosign sign is reported as a failure - $out"; fail=1
fi

# A cosign that fails the way an unreachable transparency log does.
cat > "$TMP/cosign" <<'EOF2'
#!/usr/bin/env bash
echo "Error: signing [...]: signing digest: signing bundle: error signing bundle" >&2
exit 1
EOF2
chmod +x "$TMP/cosign"

out="$(PATH="$TMP:$PATH" "$CHECK" monitor "$FAKE_DIGEST" false 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
  echo "FAIL: a failing cosign sign is reported as a success"; fail=1
elif ! printf '%s' "$out" | grep -q "::error::cosign sign failed for the monitor image"; then
  echo "FAIL: a signing failure's error does not name signing - got: $out"; fail=1
elif printf '%s' "$out" | grep -q "no digest"; then
  echo "FAIL: a signing failure's error blames the digest instead of signing - got: $out"; fail=1
else
  echo "ok: a signing failure's error names signing, not the digest"
fi

out="$(PATH="$TMP:$PATH" "$CHECK" server "" true 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
  echo "FAIL: an empty digest is accepted"; fail=1
elif ! printf '%s' "$out" | grep -q "::error::no digest for the server image"; then
  echo "FAIL: an empty digest's error does not name the image - got: $out"; fail=1
else
  echo "ok: an empty digest is refused and names the image"
fi

out="$(PATH="$TMP:$PATH" "$CHECK" server "$FAKE_DIGEST" maybe 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then echo "FAIL: an invalid tlog_upload value is accepted"; fail=1
else echo "ok: an invalid tlog_upload value is refused"; fi

if [ "$fail" -ne 0 ]; then
  echo "cosign-sign.sh does NOT behave as documented"; exit 1
fi
echo "cosign-sign.sh behaves as documented"
