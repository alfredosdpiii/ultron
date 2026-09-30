#!/usr/bin/env bash
# Shared helpers for the README demo recordings. Sourced by docs/demos/record.sh.
#
# Every demo runs isolated: a throwaway HOME, agent dir, server dir (never the default ~/.ultron/server, where a live
# session may be running) and project under $DEMO_ROOT, and memory off. The only credential copied in is models.json,
# removed again when the demo ends. Claude Code uses your real CLAUDE_CONFIG_DIR (see below); the demo project's
# Claude Code transcripts are removed from it afterwards. The recording is an asciinema cast driven by tmux send-keys;
# redact-cast.mjs strips personal details and the exit from it and fails the render if any remain, then agg draws
# the GIF.
set -euo pipefail

DEMO_ROOT=${DEMO_ROOT:-/tmp/ultron-demo}
DEMO_COLS=${DEMO_COLS:-130}
DEMO_ROWS=${DEMO_ROWS:-34}
DEMO_OUT=${DEMO_OUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}
# Where the provider config comes from (copied for the duration of one recording, then deleted).
DEMO_CREDS_DIR=${DEMO_CREDS_DIR:-$HOME/.ultron/agent}
# Claude Code keeps its login under CLAUDE_CONFIG_DIR; the demos point it at the real one instead of copying tokens
# (a copied OAuth refresh token would rotate and log the real one out).
DEMO_CLAUDE_CONFIG_DIR=${DEMO_CLAUDE_CONFIG_DIR:-$HOME/.claude}
# The release shown, installed under a neutral prefix so tracebacks and paths in the recording name no home directory.
ULTRON_VERSION=${ULTRON_VERSION:-$(ultron --version 2>/dev/null || echo latest)}
DEMO_PREFIX=${DEMO_PREFIX:-/tmp/ultron-demo-bin}
AGG=${AGG:-$(command -v agg || echo "$HOME/.cargo/bin/agg")}
SESSION=ultron-demo

demo_cleanup() {
	tmux kill-session -t "$SESSION" 2>/dev/null || true
	tmux -L ultron-demo-inner kill-server 2>/dev/null || true
	rm -f "$DEMO_ROOT/agent/models.json" "$DEMO_ROOT/agent/auth.json"
	# Claude Code's transcripts of the demo project (its directory name is the project path with / as -).
	local transcripts="$DEMO_CLAUDE_CONFIG_DIR/projects/${DEMO_ROOT//\//-}-project"
	[[ -d $transcripts ]] && rm -rf "${transcripts:?}"
	return 0
}

# demo_env <default-model> : fresh isolated environment and exports for the recorded process.
demo_env() {
	local model=$1
	demo_cleanup
	rm -rf "${DEMO_ROOT:?}"
	mkdir -p "$DEMO_ROOT/home" "$DEMO_ROOT/agent" "$DEMO_ROOT/server" "$DEMO_ROOT/project"
	chmod 700 "$DEMO_ROOT/agent" "$DEMO_ROOT/server"
	install -m 600 "$DEMO_CREDS_DIR/models.json" "$DEMO_ROOT/agent/models.json"
	trap demo_cleanup EXIT
	local provider=${model%%/*} id=${model#*/}
	cat >"$DEMO_ROOT/agent/settings.json" <<JSON
{
  "defaultProvider": "$provider",
  "defaultModel": "$id",
  "defaultThinkingLevel": "low",
  "defaultProjectTrust": "always",
  "theme": "dark",
  "hideThinkingBlock": true,
  "lastChangelogVersion": "99.0.0"
}
JSON
	if [[ $("$DEMO_PREFIX/bin/ultron" --version 2>/dev/null) != "$ULTRON_VERSION" ]]; then
		npm install -g --silent --ignore-scripts --prefix "$DEMO_PREFIX" "ultron-agent@$ULTRON_VERSION"
	fi
	export DEMO_ENV="env -i PATH=$DEMO_PREFIX/bin:$PATH TERM=xterm-256color COLORTERM=truecolor LANG=C.UTF-8 HOME=$DEMO_ROOT/home \
USER=demo LOGNAME=demo HOSTNAME=demo PS1='\$ ' SHELL=/bin/bash ULTRON_CODING_AGENT_DIR=$DEMO_ROOT/agent \
ULTRON_SERVER_DIR=$DEMO_ROOT/server ULTRON_HINDSIGHT_URL=off ULTRON_SKIP_VERSION_CHECK=1 \
CLAUDE_CONFIG_DIR=$DEMO_CLAUDE_CONFIG_DIR ${DEMO_EXTRA_ENV:-}"
}

# demo_start <cast-name> <command> : start recording <command> in the demo project inside a detached tmux session.
demo_start() {
	local name=$1 command=$2
	CAST="$DEMO_ROOT/$name.cast"
	DEMO_T0=$(date +%s.%N)
	unset DEMO_CUT
	tmux new-session -d -s "$SESSION" -x "$DEMO_COLS" -y "$DEMO_ROWS" -c "$DEMO_ROOT/project" \
		"$DEMO_ENV asciinema rec -q --overwrite --cols $DEMO_COLS --rows $DEMO_ROWS -c \"$command\" $CAST"
	tmux set-option -t "$SESSION" status off >/dev/null
}

# tmux on the server the helpers drive: the recording's, or (TMUX_L set) an inner one shown inside the recording.
tm() { if [[ -n ${TMUX_L:-} ]]; then tmux -L "$TMUX_L" "$@"; else tmux "$@"; fi; }
screen() { tm capture-pane -p -t "${1:-$SESSION}" 2>/dev/null || true; }

# wait_for <regex> [timeout-seconds] [target]
wait_for() {
	local pattern=$1 timeout=${2:-60} target=${3:-$SESSION} waited=0
	until screen "$target" | command grep -Eq -- "$pattern"; do
		sleep 0.5
		waited=$((waited + 1))
		if ((waited > timeout * 2)); then
			echo "timed out waiting for /$pattern/ in $target" >&2
			screen "$target" >&2
			return 1
		fi
	done
}

# type_text <text> [target] : type like a person (the GIF shows the prompt being written).
type_text() {
	local text=$1 target=${2:-$SESSION} i
	for ((i = 0; i < ${#text}; i++)); do
		tm send-keys -t "$target" -l "${text:i:1}"
		sleep 0.02
	done
}

keys() { tm send-keys -t "$SESSION" "$@" 2>/dev/null || true; }
pause() { sleep "${1:-1}"; }

# mark_end : the GIF ends here (call it before sending the keys that quit).
mark_end() { DEMO_CUT=$(awk -v now="$(date +%s.%N)" -v t0="$DEMO_T0" 'BEGIN { print now - t0 - 1 }'); export DEMO_CUT; }

# demo_finish : wait for the recorded command to exit.
demo_finish() {
	local waited=0
	while tmux has-session -t "$SESSION" 2>/dev/null; do
		sleep 0.5
		waited=$((waited + 1))
		((waited > 60)) && tmux kill-session -t "$SESSION"
	done
	demo_cleanup
}

# render <name> [agg args...] : redact, render the cast to docs/demos/<name>.gif, and check it.
render() {
	local name=$1
	shift
	local cast="$DEMO_ROOT/$name.cast"
	node "$DEMO_OUT/redact-cast.mjs" "$cast"
	"$AGG" --theme github-dark --font-size 14 --line-height 1.3 --idle-time-limit 1.5 --last-frame-duration 4 "$@" \
		"$cast" "$DEMO_OUT/$name.gif"
	node "$DEMO_OUT/redact-cast.mjs" --check "$cast"
	ls -la "$DEMO_OUT/$name.gif"
}

# wait_quiet [seconds-still] [timeout] : wait until the screen has not changed for a while (the turn is over).
wait_quiet() {
	local still=${1:-4} timeout=${2:-180} last="" now same=0 waited=0
	while ((waited < timeout)); do
		sleep 1
		waited=$((waited + 1))
		now=$(screen | md5sum)
		if [[ $now == "$last" ]]; then
			same=$((same + 1))
			((same >= still)) && return 0
		else
			same=0
			last=$now
		fi
	done
	echo "screen never settled" >&2
	return 1
}
