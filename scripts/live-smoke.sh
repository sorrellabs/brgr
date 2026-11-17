#!/usr/bin/env bash
# Nightly live smoke: one real bounded task through every tier 1 harness.
#
# This reproduces the manual pre-release round (AGENTS.md, Releasing step 3)
# without a person: build brgr from this checkout, register Claude Code, Codex
# CLI, Cursor CLI and Devin CLI against a fresh control home, then run one tiny
# task per harness in pane mode inside a dedicated Herdr workspace, read the
# sealed result, accept it, and check that the pane closed and the worktree was
# removed. A harness that fails is named and makes the script exit non-zero.
#
# `.github/workflows/live-smoke.yml` only calls this script, so it reads
# identically when run by hand:
#
#   scripts/live-smoke.sh --check          # what would run, no harness called
#   scripts/live-smoke.sh --artifacts /tmp/live-smoke
#
# Pane mode needs a verified Herdr caller. When this runs outside Herdr (a
# workflow job shell) it creates its own workspace, runs itself in that
# workspace's root pane, and closes the workspace afterwards. When it already
# runs inside a Herdr pane it uses that pane and creates nothing.
#
# The harness CLIs and their logins stay on this machine. Nothing here reads a
# repository secret, and nothing it writes for an issue is taken from a harness
# screen: brgr's error text is cut at its own free-text markers and home paths
# are masked first. Raw output stays under the temporary root, which is never
# uploaded; only the screen trace, the screens logs and the versions are.
#
# Environment (all optional):
#   BRGR_BIN                       brgr binary to test; default: built here
#   BRGR_LIVE_SMOKE_PROFILE        cargo profile for the build (release)
#   BRGR_LIVE_SMOKE_HARNESSES      harness ids (local.claude-code local.codex
#                                  local.cursor-cli local.devin)
#   BRGR_LIVE_SMOKE_MODEL_CLAUDE   cheapest model for Claude Code (haiku)
#   BRGR_LIVE_SMOKE_MODEL_CODEX    cheapest model for Codex CLI (gpt-6-luna)
#   BRGR_LIVE_SMOKE_MODEL_CURSOR   cheapest model for Cursor CLI (composer-2.5)
#   BRGR_LIVE_SMOKE_MODEL_DEVIN    model for Devin CLI (empty: its own setting)
#   BRGR_LIVE_SMOKE_SKIP_UPDATE    "1" to test the installed versions as-is
#   BRGR_LIVE_SMOKE_TASK_DEADLINE  seconds a task may take (900)
#   BRGR_LIVE_SMOKE_WATCH_TIMEOUT  seconds `brgr watch` may take (1200)
#   BRGR_LIVE_SMOKE_CLEANUP_WAIT   seconds to wait for the pane close and the
#                                  worktree removal after an accept (180)
#   BRGR_LIVE_SMOKE_DELEGATE_TIMEOUT  seconds the pane run may take (3600)
#   BRGR_LIVE_SMOKE_KEEP           "1" to keep the temporary root
#
# Usage: live-smoke.sh [--artifacts DIR] [--root DIR] [--repo DIR] [--brgr PATH]
#                      [--check] [--inside-pane] [--help]

set -euo pipefail

readonly OWNER_ID="codex:brgr-live-smoke"
readonly SESSION_ID="brgr-live-smoke"
# The last line of the task repository's README.md. A result that names it
# proves the harness read the file rather than answering from context.
readonly SENTINEL="LIVE-SMOKE-OK"
readonly OBJECTIVE="Read README.md in this repository and reply with its last line verbatim. Do not modify any file."

HARNESSES="${BRGR_LIVE_SMOKE_HARNESSES:-local.claude-code local.codex local.cursor-cli local.devin}"
MODEL_CLAUDE="${BRGR_LIVE_SMOKE_MODEL_CLAUDE-haiku}"
MODEL_CODEX="${BRGR_LIVE_SMOKE_MODEL_CODEX-gpt-6-luna}"
MODEL_CURSOR="${BRGR_LIVE_SMOKE_MODEL_CURSOR-composer-2.5}"
MODEL_DEVIN="${BRGR_LIVE_SMOKE_MODEL_DEVIN-}"
SKIP_UPDATE="${BRGR_LIVE_SMOKE_SKIP_UPDATE:-0}"
TASK_DEADLINE="${BRGR_LIVE_SMOKE_TASK_DEADLINE:-900}"
WATCH_TIMEOUT="${BRGR_LIVE_SMOKE_WATCH_TIMEOUT:-1200}"
DELEGATE_TIMEOUT="${BRGR_LIVE_SMOKE_DELEGATE_TIMEOUT:-3600}"
KEEP="${BRGR_LIVE_SMOKE_KEEP:-0}"
CLEANUP_WAIT="${BRGR_LIVE_SMOKE_CLEANUP_WAIT:-180}"
PROFILE="${BRGR_LIVE_SMOKE_PROFILE:-release}"

SCRIPT_PATH=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

ARTIFACTS=""
ROOT=""
BRGR=""
BRGR_EXPLICIT=0
CHECK_ONLY=0
INSIDE_PANE=0
CREATED_WORKSPACE=""

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$SCRIPT_PATH"
}

die() {
    printf 'live-smoke: %s\n' "$*" >&2
    exit 2
}

log() {
    printf 'live-smoke: %s\n' "$*" >&2
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# Make a brgr failure safe to print or file. brgr quotes a pane's screen after
# `last screen:`, and a worker's output after `last stderr:` and
# `last error event:`; a failure class in the error memo is cut at those marks
# and at the first ` | `. Do the same here, then mask this machine's paths and
# any id, and keep one bounded line.
sanitize() {
    local text=$1 marker
    text=${text//$'\n'/ }
    for marker in " last screen:" " last stderr:" " last error event:" \
        " no brgr rule answers:" " | "; do
        text=${text%%"$marker"*}
    done
    if [ -n "${HOME:-}" ]; then
        text=${text//$HOME/\~}
    fi
    text=$(printf '%s' "$text" |
        sed -e 's#/Users/[^ ]*#<path>#g' \
            -e 's#/home/[^ ]*#<path>#g' \
            -e 's#/private/[^ ]*#<path>#g' \
            -e 's#/var/[^ ]*#<path>#g' \
            -e 's#/tmp/[^ ]*#<path>#g' \
            -E -e 's/[0-9a-fA-F]{8}-[0-9a-fA-F-]{20,}/<id>/g' || true)
    printf '%s' "${text:0:300}"
}

# Single-quote a word for the shell that will read it back.
shq() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# Run a command with a wall-clock bound; the harness update commands have no
# timeout flag of their own. `alarm` survives `exec`, which is what `timeout`
# does, and perl is on the macOS runner while `timeout` is not.
run_bounded() {
    local seconds=$1
    shift
    if have perl; then
        perl -e 'alarm shift; exec @ARGV; exit 127' "$seconds" "$@"
    else
        "$@"
    fi
}

parse_args() {
    if [ -n "${BRGR_BIN:-}" ]; then
        BRGR="$BRGR_BIN"
        BRGR_EXPLICIT=1
    fi
    while [ $# -gt 0 ]; do
        case "$1" in
        --artifacts)
            [ $# -ge 2 ] || die "--artifacts needs a directory"
            ARTIFACTS=$2
            shift 2
            ;;
        --root)
            [ $# -ge 2 ] || die "--root needs a directory"
            ROOT=$2
            shift 2
            ;;
        --repo)
            [ $# -ge 2 ] || die "--repo needs a directory"
            REPO_ROOT=$2
            shift 2
            ;;
        --brgr)
            [ $# -ge 2 ] || die "--brgr needs a path"
            BRGR=$2
            BRGR_EXPLICIT=1
            shift 2
            ;;
        --check)
            CHECK_ONLY=1
            shift
            ;;
        --inside-pane)
            INSIDE_PANE=1
            shift
            ;;
        --help | -h)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
        esac
    done
    if [ -z "$ROOT" ]; then
        ROOT=$(mktemp -d "${TMPDIR:-/tmp}/brgr-live-smoke.XXXXXX")
    fi
    if [ -z "$ARTIFACTS" ]; then
        ARTIFACTS="$ROOT/artifacts"
    fi
    mkdir -p "$ROOT" "$ARTIFACTS"
    if [ -z "$BRGR" ]; then
        BRGR="$REPO_ROOT/target/$PROFILE/brgr"
    fi
}

require_checkout() {
    [ -d "$REPO_ROOT" ] || die "no repository at $REPO_ROOT"
    [ -f "$REPO_ROOT/scripts/live-smoke.sh" ] || die "this is not a brgr checkout: $REPO_ROOT"
}

require_tools() {
    local name
    for name in git jq cargo herdr; do
        have "$name" || die "missing required command: $name"
    done
    require_checkout
}

model_for() {
    case "$1" in
    local.claude-code) printf '%s' "$MODEL_CLAUDE" ;;
    local.codex) printf '%s' "$MODEL_CODEX" ;;
    local.cursor-cli) printf '%s' "$MODEL_CURSOR" ;;
    local.devin) printf '%s' "$MODEL_DEVIN" ;;
    *) printf '%s' "" ;;
    esac
}

short_name() {
    printf '%s' "${1#local.}"
}

executable_for() {
    case "$1" in
    local.claude-code) printf '%s' "claude" ;;
    local.codex) printf '%s' "codex" ;;
    local.cursor-cli)
        if have cursor-agent; then
            printf '%s' "cursor-agent"
        else
            printf '%s' "cursor"
        fi
        ;;
    local.devin) printf '%s' "devin" ;;
    *) printf '%s' "" ;;
    esac
}

show_plan() {
    local harness model executable
    printf 'repo: %s\n' "$REPO_ROOT"
    printf 'brgr: %s\n' "$BRGR"
    printf 'root: %s\n' "$ROOT"
    printf 'artifacts: %s\n' "$ARTIFACTS"
    for harness in $HARNESSES; do
        model=$(model_for "$harness")
        executable=$(executable_for "$harness")
        [ -n "$executable" ] || die "unknown harness: $harness"
        if have "$executable"; then
            printf 'harness: %s executable=%s model=%s\n' \
                "$harness" "$(command -v "$executable")" "${model:-<devin default>}"
        else
            printf 'harness: %s executable=%s MISSING model=%s\n' \
                "$harness" "$executable" "${model:-<devin default>}"
        fi
    done
}

build_brgr() {
    if [ "$BRGR_EXPLICIT" = "1" ]; then
        if [ ! -x "$BRGR" ]; then
            die "the brgr binary at $BRGR is not executable"
        fi
        log "using the passed brgr at $BRGR"
        return 0
    fi
    log "building brgr ($PROFILE) from $REPO_ROOT"
    if ! (cd "$REPO_ROOT" && cargo build -p brgr-cli --profile "$PROFILE" --locked); then
        die "building brgr failed"
    fi
    if [ ! -x "$BRGR" ]; then
        die "the build did not produce $BRGR"
    fi
}

record_version_line() {
    printf '%s\n' "$*" >>"$ARTIFACTS/versions.txt"
}

record_versions_header() {
    local head herdr_version brgr_version
    head=$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || printf '%s' unknown)
    : >"$ARTIFACTS/versions.txt"
    # `brgr --version` prints "brgr 2.15.24"; record it as printed. `|| true`
    # keeps a SIGPIPE from `head` from failing the assignment under pipefail.
    brgr_version=$("$BRGR" --version 2>/dev/null | head -1 || true)
    record_version_line "${brgr_version:-brgr unknown} ($head)"
    if herdr_version=$(herdr --version 2>/dev/null | head -1 || true) && [ -n "$herdr_version" ]; then
        record_version_line "$(sanitize "$herdr_version")"
    fi
}

prepare_repo() {
    local repo="$ROOT/repo"
    rm -rf "$repo"
    mkdir -p "$repo"
    (
        cd "$repo"
        git init -q -b main
        printf '%s\n' \
            '# live smoke repository' \
            '' \
            'This repository exists only for the nightly brgr live smoke.' \
            '' \
            "$SENTINEL" >README.md
        git add README.md
        git -c user.name='brgr live smoke' \
            -c user.email='brgr-live-smoke@example.invalid' \
            commit -q -m 'live smoke fixture'
    )
}

brgr_cmd() {
    "$BRGR" --home "$ROOT/home" "$@"
}

run_one() {
    local harness=$1 harness_exe short model version repo task detail outcome
    short=$(short_name "$harness")
    model=$(model_for "$harness")
    LAST_KIND=""
    LAST_CLASS=""
    mkdir -p "$ROOT/$short"

    harness_exe=$(command -v "$(executable_for "$harness")" 2>/dev/null || true)
    if [ -z "$harness_exe" ]; then
        LAST_KIND="harness_missing"
        LAST_CLASS="the harness executable is not installed on the runner"
        return 1
    fi
    version=$("$harness_exe" --version 2>&1 | head -1 || true)

    if [ "$SKIP_UPDATE" != "1" ]; then
        log "$short: updating from $(sanitize "$version")"
        if ! update_harness "$short" "$harness_exe" >"$ROOT/$short/update.log" 2>&1; then
            LAST_KIND="update"
            LAST_CLASS="the harness could not update itself"
            record_version_line "$short update=failed $(sanitize "$version")"
            return 1
        fi
        version=$("$harness_exe" --version 2>&1 | head -1 || true)
        record_version_line "$short update=ok $(sanitize "$version")"
    else
        record_version_line "$short update=skipped $(sanitize "$version")"
    fi
    printf '%s\n' "$(sanitize "$version")" >"$ARTIFACTS/$short.version"

    log "$short: registering $(sanitize "$version")"
    if ! register_harness "$harness" "$harness_exe" "$model" "$ROOT/$short"; then
        LAST_KIND="register"
        LAST_CLASS=$(sanitize "$(tail -1 "$ROOT/$short/register.err" 2>/dev/null || true)")
        return 1
    fi

    log "$short: starting one pane-mode task"
    repo=$ROOT/repo
    if ! task=$(start_task "$harness" "$model" "$repo" "$ROOT/$short"); then
        LAST_KIND="start"
        LAST_CLASS=$(sanitize "$(tail -1 "$ROOT/$short/run.err" 2>/dev/null || true)")
        return 1
    fi

    log "$short: waiting for the sealed result ($task)"
    if ! brgr_cmd --json watch "$task" --timeout-seconds "$WATCH_TIMEOUT" \
        >"$ROOT/$short/watch.jsonl" 2>"$ROOT/$short/watch.err"; then
        LAST_KIND="timeout"
        LAST_CLASS=$(sanitize "$(tail -1 "$ROOT/$short/watch.err" 2>/dev/null || true)")
        return 1
    fi

    if ! detail=$(brgr_cmd --json result "$task" 2>"$ROOT/$short/result.err"); then
        LAST_KIND="result"
        LAST_CLASS=$(sanitize "$(tail -1 "$ROOT/$short/result.err" 2>/dev/null || true)")
        return 1
    fi
    printf '%s\n' "$detail" >"$ROOT/$short/result.json"
    outcome=$(printf '%s' "$detail" | jq -r '.result.outcome // "unknown"')
    if [ "$outcome" != "candidate" ]; then
        LAST_KIND="run_$outcome"
        LAST_CLASS=$(sanitize "$(printf '%s' "$detail" | jq -r '.result.error // ""')")
        return 1
    fi

    if ! result_names_the_sentinel "$detail"; then
        LAST_KIND="artifact"
        LAST_CLASS="the sealed result did not contain the README last line"
        return 1
    fi

    log "$short: accepting $task"
    if ! brgr_cmd --json accept "$task" --reason "live smoke: the harness returned the README last line" \
        >"$ROOT/$short/accept.json" 2>"$ROOT/$short/accept.err"; then
        LAST_KIND="accept"
        LAST_CLASS=$(sanitize "$(tail -1 "$ROOT/$short/accept.err" 2>/dev/null || true)")
        return 1
    fi
    if [ "$(jq -r '.verdict // ""' "$ROOT/$short/accept.json")" != "accepted" ]; then
        LAST_KIND="accept"
        LAST_CLASS="brgr did not record the acceptance"
        return 1
    fi

    log "$short: checking the pane closed and the worktree was removed"
    if ! verify_cleanup "$task" "$ROOT/$short"; then
        LAST_KIND="cleanup"
        LAST_CLASS=$(sanitize "$(cat "$ROOT/$short/cleanup.txt" 2>/dev/null || true)")
        return 1
    fi

    printf 'OK %s candidate accepted pane=closed worktree=removed\n' "$harness" \
        >>"$ARTIFACTS/summary.txt"
    return 0
}

update_harness() {
    local short=$1 harness_exe=$2
    case "$short" in
    devin) run_bounded 900 "$harness_exe" update --process-model in-process </dev/null ;;
    *) run_bounded 900 "$harness_exe" update </dev/null ;;
    esac
}

register_harness() {
    local harness=$1 harness_exe=$2 model=$3 out_dir=$4
    local args=()
    args=("$BRGR" --home "$ROOT/home" --json harness add "$harness_exe")
    if [ -n "$model" ]; then
        args+=(--model "$model")
    fi
    "${args[@]}" >"$out_dir/register.json" 2>"$out_dir/register.err"
}

start_task() {
    local harness=$1 model=$2 repo=$3 out_dir=$4
    local args=()
    args=("$BRGR" --home "$ROOT/home" --json run "$OBJECTIVE"
        --harness "$harness" --workspace "$repo" --mode pane
        --deadline-seconds "$TASK_DEADLINE")
    if [ -n "$model" ]; then
        args+=(--model "$model")
    fi
    if ! "${args[@]}" >"$out_dir/run.json" 2>"$out_dir/run.err"; then
        return 1
    fi
    jq -er '.task_id' "$out_dir/run.json"
}

result_names_the_sentinel() {
    local detail=$1 text
    text=$(printf '%s' "$detail" | jq -r '.artifacts[0].text // empty')
    [ -n "$text" ] || return 1
    # A bash match, not `printf | grep -q`: `grep -q` closes the pipe as soon as
    # it matches, and under pipefail that upstream write can fail the check.
    [[ ${text,,} == *"${SENTINEL,,}"* ]]
}

# The run is only clean when brgr closed its pane and removed the worktree.
verify_cleanup() {
    local task=$1 out_dir=$2 deadline present cleanup pane
    deadline=$(($(date +%s) + CLEANUP_WAIT))
    while :; do
        if ! brgr_cmd --json status "$task" >"$out_dir/status.json" 2>"$out_dir/status.err"; then
            return 1
        fi
        # `//` would also replace a JSON false, and `workspace_present: false`
        # is the passing value. Read it as written and treat a missing key as
        # still present.
        present=$(jq -r 'if has("workspace_present") then .workspace_present else true end' \
            "$out_dir/status.json")
        cleanup=$(jq -r 'if (.session | type) == "object" then .session.cleanup // "none" else "none" end' \
            "$out_dir/status.json")
        pane=$(jq -r 'if (.session | type) == "object" then .session.pane // empty else empty end' \
            "$out_dir/status.json")
        if [ "$present" = "false" ] && [ "$cleanup" = "closed" ]; then
            if [ -n "$pane" ] && herdr pane get "$pane" >/dev/null 2>&1; then
                break
            fi
            return 0
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            break
        fi
        sleep 3
    done
    printf 'workspace_present=%s session_cleanup=%s\n' "$present" "$cleanup" \
        >"$out_dir/cleanup.txt"
    return 1
}

collect_artifacts() {
    local file
    if [ -d "$ROOT/home/runs" ]; then
        for file in "$ROOT/home/runs"/*.screens.log "$ROOT/home/runs"/*.screens.trace; do
            [ -f "$file" ] || continue
            # A screens log holds the rule name and the keys brgr pressed; the
            # trace holds one digest per screen decision. Neither holds text.
            cp "$file" "$ARTIFACTS/"
        done
    fi
    [ -f "$ARTIFACTS/summary.txt" ] || : >"$ARTIFACTS/summary.txt"
    [ -f "$ARTIFACTS/versions.txt" ] || : >"$ARTIFACTS/versions.txt"
    [ -f "$ARTIFACTS/failures.jsonl" ] || : >"$ARTIFACTS/failures.jsonl"
}

# One sanitized line per failed harness for the workflow to file. It carries
# the harness, the failure kind, the harness version and the redacted error
# class, and never a prompt, report, file content or screen text.
record_failure() {
    local harness=$1 kind=$2 version=$3 class=$4
    jq -cn \
        --arg harness "$harness" \
        --arg kind "$kind" \
        --arg version "$(sanitize "$version")" \
        --arg class "$(sanitize "$class")" \
        '{harness: $harness, kind: $kind, version: $version, class: $class}' \
        >>"$ARTIFACTS/failures.jsonl"
}

run_suite() {
    local harness failures="" version
    export BRGR_HOME="$ROOT/home"
    export BRGR_OWNER_ID="$OWNER_ID"
    export BRGR_SESSION_ID="$SESSION_ID"
    # One digest per screen decision, never the screen.
    export BRGR_SCREEN_TRACE=1

    mkdir -p "$ROOT/home"
    record_versions_header
    prepare_repo
    : >"$ARTIFACTS/summary.txt"
    : >"$ARTIFACTS/failures.jsonl"

    for harness in $HARNESSES; do
        if run_one "$harness"; then
            log "$(short_name "$harness"): passed"
        else
            version="unknown"
            [ -f "$ARTIFACTS/$(short_name "$harness").version" ] &&
                version=$(cat "$ARTIFACTS/$(short_name "$harness").version")
            printf 'FAILED %s %s\n' "$harness" "$LAST_KIND" >>"$ARTIFACTS/summary.txt"
            printf 'FAILED %s: %s\n' "$(short_name "$harness")" "$LAST_KIND" >&2
            record_failure "$harness" "$LAST_KIND" "$version" "$LAST_CLASS"
            failures="$failures $(short_name "$harness"):$LAST_KIND"
        fi
    done

    collect_artifacts
    printf '\n' >&2
    cat "$ARTIFACTS/versions.txt" >&2
    if [ -n "$failures" ]; then
        log "failed harnesses:$failures"
        return 1
    fi
    log "every tier 1 harness passed"
    return 0
}

# A workflow job shell (and a plain terminal) has no verified Herdr caller, and
# pane mode refuses to guess one. Run the suite in a workspace of our own.
delegate_into_herdr() {
    local pane exit_file log_file status deadline command
    exit_file="$ROOT/delegate.exit"
    log_file="$ROOT/suite.log"
    rm -f "$exit_file"
    log "not inside Herdr; creating a workspace for the run"

    if ! herdr workspace create --label brgr-live-smoke --cwd "$REPO_ROOT" --no-focus \
        >"$ROOT/workspace.json" 2>"$ROOT/workspace.err"; then
        cat "$ROOT/workspace.err" >&2 || true
        die "could not create a Herdr workspace"
    fi
    CREATED_WORKSPACE=$(jq -er '.result.workspace.workspace_id' "$ROOT/workspace.json") ||
        die "Herdr did not report the new workspace"
    pane=$(jq -er '.result.root_pane.pane_id' "$ROOT/workspace.json") ||
        die "Herdr did not report the workspace's root pane"

    command="$(shq "$SCRIPT_PATH") --inside-pane --root $(shq "$ROOT")"
    command="$command --artifacts $(shq "$ARTIFACTS") --repo $(shq "$REPO_ROOT")"
    command="$command --brgr $(shq "$BRGR")"
    # The pane's shell, not this one, expands `$?`: this runs after the suite
    # and records its status for the waiting loop below.
    command="$command > $(shq "$log_file") 2>&1; printf '%s\\n' \"\$?\" > $(shq "$exit_file")"

    if ! herdr pane run "$pane" "$command" >/dev/null 2>"$ROOT/delegate.err"; then
        cat "$ROOT/delegate.err" >&2 || true
        die "could not start the suite in the Herdr pane"
    fi

    deadline=$(($(date +%s) + DELEGATE_TIMEOUT))
    while [ ! -f "$exit_file" ]; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            die "the pane run did not finish within $DELEGATE_TIMEOUT seconds"
        fi
        sleep 5
    done
    status=$(tr -dc '0-9' <"$exit_file")
    [ -n "$status" ] || status=1
    log "suite finished with status $status"
    return "$status"
}

cleanup() {
    local status=$?
    if [ -n "$CREATED_WORKSPACE" ]; then
        herdr workspace close "$CREATED_WORKSPACE" >/dev/null 2>&1 || true
        CREATED_WORKSPACE=""
    fi
    if [ "$KEEP" != "1" ]; then
        case "$ARTIFACTS" in
        "$ROOT"/*)
            # The artifacts are the only thing worth keeping from a run; the
            # throwaway home and repository are not.
            rm -rf "${ROOT:?}/home" "${ROOT:?}/repo"
            ;;
        *) rm -rf "$ROOT" ;;
        esac
    fi
    exit "$status"
}

main() {
    parse_args "$@"
    trap cleanup EXIT INT TERM
    if [ "$CHECK_ONLY" = "1" ]; then
        # A plan calls no harness and needs no Herdr, so it works anywhere.
        require_checkout
        show_plan
        return 0
    fi
    if [ "$INSIDE_PANE" = "1" ] && [ "${HERDR_ENV:-}" != "1" ]; then
        die "this pane has no Herdr environment (HERDR_ENV is not 1)"
    fi
    require_tools
    if [ "$INSIDE_PANE" != "1" ] && [ "${HERDR_ENV:-}" != "1" ]; then
        build_brgr
        delegate_into_herdr
        return $?
    fi
    build_brgr
    run_suite
}

main "$@"
