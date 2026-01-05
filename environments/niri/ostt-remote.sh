#!/usr/bin/env bash
set -euo pipefail

OSTT_BIN="${OSTT_BIN:-ostt}"
OSTT_CLASS="${OSTT_CLASS:-com.local.ostt}"
OSTT_SOCKET="${OSTT_REMOTE_SOCKET:-${XDG_RUNTIME_DIR:-/tmp}/ostt.sock}"
OSTT_LAUNCH_CMD="${OSTT_LAUNCH_CMD:-ghostty --class ${OSTT_CLASS} -e ${OSTT_BIN} remote}"

has_ostt_window() {
    if command -v niri >/dev/null 2>&1; then
        niri msg -j 2>/dev/null \
            | grep -Eq "\"app_id\"[[:space:]]*:[[:space:]]*\"${OSTT_CLASS}\""
    else
        return 1
    fi
}

if has_ostt_window; then
    "${OSTT_BIN}" remote complete || true
    exit 0
fi

if [ -S "${OSTT_SOCKET}" ]; then
    if "${OSTT_BIN}" remote ping >/dev/null 2>&1; then
        "${OSTT_BIN}" remote complete || true
        exit 0
    fi
fi

exec bash -c "${OSTT_LAUNCH_CMD}"
