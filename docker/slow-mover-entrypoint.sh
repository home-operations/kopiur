#!/kopiur-e2e/busybox sh
# E2E FIXTURE ONLY — never shipped in a release image.
#
# Entrypoint of the `mover-slow` image (docker/Dockerfile.mover-slow): sleep a
# deterministic number of seconds, then `exec` the REAL mover with the original
# argv. That makes a mover Job occupy its concurrency slot for a predictable
# window, which is what the concurrency-limit / Replace-cancels-running-backup
# e2e scenarios need — a real backup against the kind node's hostPath repo
# finishes in under a second, far too fast to observe a queue.
#
# The real mover image is distroless (no shell), so the image copies a static
# busybox in and runs this script with `busybox sh`. Nothing else about the
# image changes: same kopiur-mover, kopia and rclone binaries, same uid.
#
# Inputs (both read from the container env, so a scenario can set them per
# repository by adding the key to that repository's credentials Secret — mover
# pods get `envFrom: secretRef` over the whole Secret, and pod env overrides the
# image's ENV default):
#
#   KOPIUR_E2E_MOVER_DELAY_SECONDS  seconds to sleep before exec'ing the mover.
#                                   Defaults to the image-baked value (60).
#                                   `0` disables the delay entirely.
#   KOPIUR_E2E_MOVER_DELAY_OPS      OPTIONAL comma/space-separated list of
#                                   work-spec operation keys to delay (e.g.
#                                   `snapshot,restore`). Unset/empty ⇒ delay
#                                   EVERY operation. Matched against the
#                                   `"operation":{"<key>":` discriminant of the
#                                   inline work spec (KOPIUR_WORK_SPEC), which
#                                   is the controller↔mover wire contract
#                                   (crates/mover/src/workspec/mod.rs).
#
# Signals: `sh` blocked in a foreground `sleep` does NOT react to SIGTERM (its
# own default disposition is deferred until the child reaps), so a pod deleted
# mid-sleep would sit out the whole terminationGracePeriod and only die to
# SIGKILL — measured at the full grace period before this was handled. Since a
# cancellation scenario is precisely what this fixture exists to support, the
# sleep runs in the BACKGROUND under a TERM/INT trap that kills it and exits
# 143, so deletion terminates the container promptly. After the exec, the mover
# is PID 1 and handles its own signals as usual.

set -eu

MOVER=/usr/local/bin/kopiur-mover
# Only the busybox BINARY is copied into the distroless image — none of the
# usual /bin/<applet> symlinks exist and PATH resolves nothing — so every applet
# (`sleep`) must be invoked through it explicitly. Shell builtins (echo, case,
# for) are unaffected; the operation filter below deliberately uses IFS word
# splitting rather than `tr` for the same reason.
BUSYBOX=/kopiur-e2e/busybox

delay="${KOPIUR_E2E_MOVER_DELAY_SECONDS:-60}"
ops="${KOPIUR_E2E_MOVER_DELAY_OPS:-}"
spec="${KOPIUR_WORK_SPEC:-}"

case "$delay" in
    '' | *[!0-9]*)
        echo "[slow-mover] KOPIUR_E2E_MOVER_DELAY_SECONDS='$delay' is not a whole number of \
seconds — set it to a non-negative integer (0 disables the delay)" >&2
        exit 64
        ;;
esac

# True when this run's operation is in the filter (or no filter is set). The
# subshell keeps the IFS override — which is what splits the comma/space list
# without `tr` — from leaking into the exec'd mover's environment.
should_delay() {
    [ -n "$ops" ] || return 0
    (
        IFS=', '
        for op in $ops; do
            case "$spec" in
                *"\"operation\":{\"$op\":"*) exit 0 ;;
            esac
        done
        exit 1
    )
}

if [ "$delay" != "0" ] && should_delay; then
    echo "[slow-mover] sleeping ${delay}s before exec ${MOVER} (ops filter: ${ops:-<all>})" >&2
    # Background + trap so a deleted pod dies now rather than at SIGKILL (see the
    # signals note above). `wait` is interrupted by a trapped signal; `|| true`
    # keeps `set -e` from turning that interruption into an exit before the trap.
    "$BUSYBOX" sleep "$delay" &
    sleeper=$!
    trap '"$BUSYBOX" kill -TERM "$sleeper" 2>/dev/null || true; exit 143' INT TERM
    wait "$sleeper" || true
    trap - INT TERM
else
    echo "[slow-mover] no delay (delay=${delay}s, ops filter: ${ops:-<all>})" >&2
fi

exec "$MOVER" "$@"
