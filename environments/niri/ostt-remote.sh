#!/usr/bin/env bash
set -euo pipefail

OSTT_BIN="${OSTT_BIN:-ostt}"
OSTT_DEV_BIN="${OSTT_DEV_BIN:-${HOME}/dev/ostt/target/debug/ostt}"

if [ "${OSTT_DEV:-0}" = "1" ]; then
    if [ ! -x "${OSTT_DEV_BIN}" ]; then
        if command -v notify-send >/dev/null 2>&1; then
            notify-send "ostt dev binary not found" "Run: cd ~/dev/ostt && cargo build"
        fi
        echo "ostt dev binary not found: ${OSTT_DEV_BIN}" >&2
        exit 1
    fi
    OSTT_BIN="${OSTT_DEV_BIN}"
fi

OSTT_CLASS="${OSTT_CLASS:-com.local.ostt}"
OSTT_SOCKET="${OSTT_REMOTE_SOCKET:-${XDG_RUNTIME_DIR:-/tmp}/ostt.sock}"
OSTT_LAUNCH_CMD="${OSTT_LAUNCH_CMD:-ghostty --class ${OSTT_CLASS} -e ${OSTT_BIN} remote}"
export OSTT_BIN OSTT_CLASS

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

if [ -z "${WHISPER_URL:-}" ] && [ -n "${OSTT_TRANSCRIPTION_ENDPOINT:-}" ]; then
    export WHISPER_URL="${OSTT_TRANSCRIPTION_ENDPOINT}"
fi

if [ -z "${WHISPER_MODEL:-}" ] && [ -n "${OSTT_TRANSCRIPTION_MODEL:-}" ]; then
    export WHISPER_MODEL="${OSTT_TRANSCRIPTION_MODEL}"
fi

if [ -z "${WHISPER_API_KEY:-}" ] && [ -n "${OSTT_TRANSCRIPTION_API_KEY:-}" ]; then
    export WHISPER_API_KEY="${OSTT_TRANSCRIPTION_API_KEY}"
fi

has_ostt_window() {
    if command -v niri >/dev/null 2>&1; then
        niri msg -j 2>/dev/null \
            | grep -Eq "\"app_id\"[[:space:]]*:[[:space:]]*\"${OSTT_CLASS}\""
    else
        return 1
    fi
}

if has_ostt_window; then
    if [ "${ACTION}" = "complete" ] && [ "${OUTPUT_MODE}" = "type" ]; then
        "${OSTT_BIN}" remote "${ACTION}" type || true
    else
        "${OSTT_BIN}" remote "${ACTION}" || true
    fi
    exit 0
fi

if [ -S "${OSTT_SOCKET}" ]; then
    if "${OSTT_BIN}" remote ping >/dev/null 2>&1; then
        if [ "${ACTION}" = "complete" ] && [ "${OUTPUT_MODE}" = "type" ]; then
            "${OSTT_BIN}" remote "${ACTION}" type || true
        else
            "${OSTT_BIN}" remote "${ACTION}" || true
        fi
        exit 0
    fi
fi

if [ "${ACTION}" = "cancel" ]; then
    exit 0
fi

if [ "${OUTPUT_MODE}" != "paste" ]; then
    exec bash -c "OSTT_REMOTE_OUTPUT_MODE=${OUTPUT_MODE} ${OSTT_LAUNCH_CMD}"
else
    exec bash -c "${OSTT_LAUNCH_CMD}"
fi
