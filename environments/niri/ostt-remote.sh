#!/usr/bin/env bash
set -euo pipefail

OSTT_BIN="${OSTT_BIN:-ostt}"
OSTT_DEV_BIN="${OSTT_DEV_BIN:-${HOME}/dev/ostt/target/debug/ostt}"
OSTT_DEV_BIN_MISSING=0

if [ "${OSTT_DEV:-0}" = "1" ]; then
    if [ -x "${OSTT_DEV_BIN}" ]; then
        OSTT_BIN="${OSTT_DEV_BIN}"
    else
        OSTT_DEV_BIN_MISSING=1
    fi
fi

OSTT_CLASS="${OSTT_CLASS:-com.local.ostt}"
OSTT_SOCKET="${OSTT_REMOTE_SOCKET:-${XDG_RUNTIME_DIR:-/tmp}/ostt.sock}"
OSTT_LAUNCH_CMD="${OSTT_LAUNCH_CMD:-ghostty --class ${OSTT_CLASS} -e ${OSTT_BIN} remote}"
export OSTT_BIN OSTT_CLASS

RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
if [ "${OSTT_CLASS}" = "com.local.ostt.dev" ]; then
    OSTT_PEER_CLASS="${OSTT_PEER_CLASS:-com.local.ostt}"
    OSTT_PEER_SOCKET="${OSTT_PEER_SOCKET:-${RUNTIME_DIR}/ostt.sock}"
else
    OSTT_PEER_CLASS="${OSTT_PEER_CLASS:-com.local.ostt.dev}"
    OSTT_PEER_SOCKET="${OSTT_PEER_SOCKET:-${RUNTIME_DIR}/ostt-dev.sock}"
fi

ACTION="complete"
OUTPUT_MODE="${OSTT_REMOTE_OUTPUT_MODE:-paste}"

for arg in "$@"; do
    case "$arg" in
        cancel|--cancel)
            ACTION="cancel"
            ;;
        type|typed|manual|--type)
            OUTPUT_MODE="type"
            ;;
        paste|--paste)
            OUTPUT_MODE="paste"
            ;;
    esac
done

if [ -n "${OSTT_TRANSCRIPTION_ENDPOINT:-}" ]; then
    export WHISPER_URL="${OSTT_TRANSCRIPTION_ENDPOINT}"
fi

if [ -n "${OSTT_TRANSCRIPTION_MODEL:-}" ]; then
    export WHISPER_MODEL="${OSTT_TRANSCRIPTION_MODEL}"
fi

if [ -n "${OSTT_TRANSCRIPTION_API_KEY:-}" ]; then
    export WHISPER_API_KEY="${OSTT_TRANSCRIPTION_API_KEY}"
fi

has_ostt_window() {
    local class="$1"
    if command -v niri >/dev/null 2>&1; then
        niri msg -j 2>/dev/null \
            | grep -Eq "\"app_id\"[[:space:]]*:[[:space:]]*\"${class}\""
    else
        return 1
    fi
}

send_remote() {
    local socket="$1"
    if [ -S "${socket}" ] && OSTT_REMOTE_SOCKET="${socket}" "${OSTT_BIN}" remote ping >/dev/null 2>&1; then
        if [ "${ACTION}" = "complete" ] && [ "${OUTPUT_MODE}" = "type" ]; then
            OSTT_REMOTE_SOCKET="${socket}" "${OSTT_BIN}" remote "${ACTION}" type || true
        else
            OSTT_REMOTE_SOCKET="${socket}" "${OSTT_BIN}" remote "${ACTION}" || true
        fi
        return 0
    fi
    return 1
}

handle_existing() {
    local class="$1"
    local socket="$2"
    if has_ostt_window "${class}"; then
        send_remote "${socket}" || true
        exit 0
    fi
    if send_remote "${socket}"; then
        exit 0
    fi
}

handle_existing "${OSTT_CLASS}" "${OSTT_SOCKET}"
handle_existing "${OSTT_PEER_CLASS}" "${OSTT_PEER_SOCKET}"

if [ "${ACTION}" = "cancel" ]; then
    exit 0
fi

if [ "${OSTT_DEV_BIN_MISSING}" = "1" ]; then
    if command -v notify-send >/dev/null 2>&1; then
        notify-send "ostt dev binary not found" "Run: cd ~/dev/ostt && cargo build"
    fi
    echo "ostt dev binary not found: ${OSTT_DEV_BIN}" >&2
    exit 1
fi

if [ "${OUTPUT_MODE}" != "paste" ]; then
    exec bash -c "OSTT_REMOTE_OUTPUT_MODE=${OUTPUT_MODE} ${OSTT_LAUNCH_CMD}"
else
    exec bash -c "${OSTT_LAUNCH_CMD}"
fi
