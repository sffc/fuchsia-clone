#!/bin/bash
# Copyright 2019 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

### common methods for metrics collection
### this script assumes that vars.sh has already been sourced, since it
### depends on FUCHSIA_DIR being defined correctly.

declare -r GA_PROPERTY_ID="UA-127897021-6"
declare -r TRACK_ALL_ARGS="set"
declare -r TRACK_RESULTS="set,build"
declare -r DEBUG_LOG_CONFIG="/tmp/.fx_metrics_debugfile"

# To properly enable unit testing, METRICS_CONFIG is not read-only
METRICS_CONFIG="${FUCHSIA_DIR}/.fx-metrics-config"

_METRICS_DEBUG=0
_METRICS_DEBUG_LOG_FILE=""
_METRICS_USE_VALIDATION_SERVER=0

INIT_WARNING=$'Please opt in or out of fx metrics collection.\n'
INIT_WARNING+=$'You will receive this warning until an option is selected.\n'
INIT_WARNING+=$'To check what data we collect, run `fx metrics`\n'
INIT_WARNING+=$'To opt in or out, run `fx metrics <enable|disable>\n'

function metrics-read-config {
  METRICS_UUID=""
  METRICS_ENABLED=0
  if [[ ! -f "${METRICS_CONFIG}" ]]; then
    return 1
  fi
  source "${METRICS_CONFIG}"
  if [[ $METRICS_ENABLED == 1 && -z "$METRICS_UUID" ]]; then
    METRICS_ENABLED=0
    return 1
  fi
  return 0
}

function metrics-write-config {
  enabled=$1
  if [[ "$enabled" -eq "1" ]]; then
    uuid="$2"
  fi
  local -r tempfile="$(mktemp)"

  # Exit trap to clean up temp file
  trap "[[ -f \"${tempfile}\" ]] && rm -f \"${tempfile}\"" EXIT

  {
    echo "# Autogenerated config file for fx metrics. Run 'fx help metrics' for more information."
    echo "METRICS_ENABLED=${enabled}"
    echo "METRICS_UUID=\"${uuid}\""
  } >> "${tempfile}"
  # Only rewrite the config file if content has changed
  if ! cmp --silent "${tempfile}" "${METRICS_CONFIG}" ; then
    mv -f "${tempfile}" "${METRICS_CONFIG}"
  fi
}

function metrics-read-and-validate {
  local hide_init_warning=$1
  if ! metrics-read-config; then
    if [[ $hide_init_warning -ne 1 ]]; then
      fx-warn "${INIT_WARNING}"
    fi
    return 1
  fi
  return 0
}

function metrics-set-debug-logfile {
  debug_logfile="$1"
  # make sure that either DEBUG_LOG_CONFIG is writable or it doesn't exist
  # and its directory is writable
  if [[ -w "${DEBUG_LOG_CONFIG}" || \
        ( ! -f "${DEBUG_LOG_CONFIG}" &&
          -w "$(dirname "${DEBUG_LOG_CONFIG}")" ) ]]; then
    echo "$debug_logfile" > "$DEBUG_LOG_CONFIG"
    return 0
  else
    fx-warn "Cannot persist the metrics log filename to ${DEBUG_LOG_CONFIG}"
    fx-warn "Ignoring debug logging of metrics collection"
  fi
  return 1
}

function metrics-get-debug-logfile {
  if [[ $_METRICS_DEBUG == 1 ]]; then
    echo "$_METRICS_DEBUG_LOG_FILE"
  elif [[ -f "$DEBUG_LOG_CONFIG" ]]; then
    head -1 "$DEBUG_LOG_CONFIG"
  fi
}

function metrics-maybe-log {
  local filename="$(metrics-get-debug-logfile)"
  if [[ $filename ]]; then
    if [[ ! -f "$filename" && -w $(dirname "$filename") ]]; then
      touch "$filename"
    fi
    if [[ -w "$filename" ]]; then
      TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
      shift     # remove the function name from $@
      echo "${TIMESTAMP}:" "$@" >> "$filename"
    fi
  fi
}

# Arguments:
#   - the name of the fx subcommand
#   - args of the subcommand
function track-command-execution {
  subcommand="$1"
  shift
  args="$*"

  local hide_init_warning=0
  if [[ "$subcommand" == "metrics" ]]; then
    hide_init_warning=1
  fi
  metrics-read-and-validate $hide_init_warning
  if [[ $METRICS_ENABLED == 0 ]]; then
    return 0
  fi

  # Only track arguments to the subcommands in $TRACK_ALL_ARGS
  if [[ $TRACK_ALL_ARGS != *"$subcommand"* ]]; then
    args=""
  else
    # Limit to the first 100 characters of arguments.
    # The Analytics API supports up to 500 bytes, but it is likely that
    # anything larger than 100 characters is an invalid execution and/or not
    # what we want to track.
    args=${args:0:100}
  fi

  user_agent="Fuchsia-fx $(_os_data)"

  analytics_args=(
    "v=1" \
    "tid=${GA_PROPERTY_ID}" \
    "an=fx" \
    "cid=${METRICS_UUID}" \
    "t=event" \
    "ec=fx" \
    "ea=${subcommand}"\
    "el=${args}"\
    )

  curl_args=()
  for a in "${analytics_args[@]}"; do
    curl_args+=(--data-urlencode)
    curl_args+=("$a")
  done
  local url_path="/collect"
  local result=""
  if [[ $_METRICS_DEBUG == 1 && $_METRICS_USE_VALIDATION_SERVER == 1 ]]; then
    url_path="/debug/collect"
  fi
  if [[ $_METRICS_DEBUG == 1 && $_METRICS_USE_VALIDATION_SERVER == 1 ]]; then
    # if testing and not using the validation server, always return 202
    result="202"
  elif [[ $_METRICS_DEBUG == 0 || $_METRICS_USE_VALIDATION_SERVER == 1 ]]; then
    result=$(curl -s -o /dev/null -w "%{http_code}" "${curl_args[@]}" \
      -H "User-Agent: $user_agent" \
      "https://www.google-analytics.com/${url_path}")
  fi
  metrics-maybe-log event_hit "${analytics_args[@]}" "RESULT=$result"

  return 0
}

# TODO(mangini): NOOP at this moment
# Arguments:
#   - the name of the fx subcommand
#   - args of the subcommand
#   - time taken to complete (milliseconds)
#   - exit status
function track-command-finished {
  subcommand=$1
  args=$2
  timing=$3
  exit_status=$4
  if [[ $METRICS_ENABLED == 0 || $TRACK_RESULTS != *"$subcommand"* ]]; then
    return 0
  fi
}

function _os_data {
  if command -v uname >/dev/null 2>&1 ; then
    uname -rs
  else
    echo "Unknown"
  fi
}

# Args:
#   debug_log_file: string with a filename to save logs
#   use_validation_hit_server:
#          0 do not hit any Analytics server (for local tests)
#          1 use the Analytics validation Hit server (for integration tests)
#   config_file: string with a filename to save the config file. Defaults to
#          METRICS_CONFIG
function _enable_testing {
  _METRICS_DEBUG_LOG_FILE="$1"
  _METRICS_USE_VALIDATION_SERVER=$2
  if [[ $# -gt 2 ]]; then
    METRICS_CONFIG="$3"
  fi
  _METRICS_DEBUG=1
  METRICS_UUID="TEST"
}
