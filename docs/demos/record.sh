#!/usr/bin/env bash
# Records the README demo GIFs: docs/demos/record.sh [demo...]   (default: all)
#
# Needs: ultron (installed, the release being shown), asciinema 2.x, agg, tmux, git, python3; for the Claude demos a
# logged-in `claude` CLI. Each demo runs in an isolated temp environment (see lib.sh) and renders
# docs/demos/<name>.gif. Model calls are real, so output differs a little from run to run.
#
# Models: NATIVE_MODEL for the native demos, CLAUDE_MODEL for the Claude Code ones (a small model keeps plan usage low).
set -euo pipefail
cd "$(dirname "$0")"
source ./lib.sh
NATIVE_MODEL=${NATIVE_MODEL:-cliproxyapi/glm-5.3-flash}
CLAUDE_MODEL=${CLAUDE_MODEL:-haiku}

# A small Git project in $DEMO_ROOT/project with a committed baseline.
project_init() {
	cd "$DEMO_ROOT/project"
	git init -q -b main
	git config user.name Demo
	git config user.email demo@localhost
	git config commit.gpgsign false
}
project_commit() { git add -A && git commit -q -m "${1:-baseline}"; }

demo_settings() {
	demo_env "$NATIVE_MODEL"
	project_init
	echo "# demo" >README.md && project_commit
	demo_start settings "ultron"
	wait_for "glm|ultron v" 30
	pause 1.5
	type_text "/settings"; pause 0.6; keys Enter; pause 1.5
	keys Enter; pause 2                       # Models
	keys Down; pause 0.8; keys Enter; pause 2 # Frame model
	type_text "haiku"; pause 1.2; keys Enter; pause 2.5
	keys Down; pause 0.5; keys Down; pause 0.5; keys Down; pause 0.8; keys Enter; pause 1.5 # Frame thinking
	screen | command grep -q "low" && { type_text "low"; pause 0.8; }
	keys Enter; pause 3
	mark_end; keys Escape; pause 0.5; keys Escape; pause 0.5
	keys C-d; demo_finish
	render settings
}

demo_native() {
	demo_env "$NATIVE_MODEL"
	project_init
	cat >feedback.csv <<'CSV'
id,text
1,"The export button crashes the app when the report has no rows."
2,"Please add a dark mode, my eyes hurt at night."
3,"Setup took two minutes. Lovely onboarding."
4,"Dates show as 1970-01-01 after importing a CSV from Excel."
5,"Would be great to schedule reports weekly."
6,"Search is fast and finds exactly what I need."
7,"Login loops back to the sign-in page on Safari."
8,"Can we get an API for bulk uploads?"
CSV
	project_commit
	demo_start native "ultron"
	wait_for "glm" 30
	pause 1.5
	type_text "Tag each row of feedback.csv as bug, feature or praise: h = await rlm.load(...), one rlm.map over the rows (contract=str). Print the counts and the bug ids."
	pause 0.8; keys Enter
	wait_for "rlm" 60
	wait_quiet 6 240
	pause 2
	mark_end; keys C-d; pause 0.5; keys C-d; demo_finish
	render native
}

# A small Python service for the Claude Code demos.
python_project() {
	project_init
	mkdir -p src tests
	cat >src/prices.py <<'PY'
def total(items, tax_rate=0.12):
    """Sum of price * qty, plus tax."""
    subtotal = sum(item["price"] * item["qty"] for item in items)
    return round(subtotal * (1 + tax_rate), 2)


def discount(amount, percent):
    return amount - amount * percent / 100
PY
	cat >src/orders.py <<'PY'
from prices import total


def summarize(orders):
    return {order["id"]: total(order["items"]) for order in orders}
PY
	cat >CHANGELOG.md <<'MD'
# Changelog

## 1.4.0
- Weekly report scheduling.

## 1.3.2
- Fix CSV date import.
MD
	project_commit
}

demo_claude_root() {
	demo_env "$NATIVE_MODEL"
	python_project
	demo_start claude-root "ultron --claude --model claude-code/$CLAUDE_MODEL"
	wait_for "Claude Code" 40
	pause 1.5
	type_text "Spawn one sub-agent with rlm.spawn to read CHANGELOG.md and return the latest version. Meanwhile count the lines under src/ in a cell. Report both."
	pause 0.8; keys Enter
	wait_for "rlm" 90
	wait_quiet 8 300
	pause 2
	mark_end; keys C-d; pause 0.5; keys C-d; demo_finish
	render claude-root
}

demo_claude_tui() {
	demo_env "$NATIVE_MODEL"
	project_init
	cat >feedback.csv <<'CSV'
id,text
1,"The export button crashes the app when the report has no rows."
2,"Please add a dark mode, my eyes hurt at night."
3,"Setup took two minutes. Lovely onboarding."
4,"Dates show as 1970-01-01 after importing a CSV from Excel."
5,"Would be great to schedule reports weekly."
6,"Login loops back to the sign-in page on Safari."
CSV
	project_commit
	# Claude Code asks once whether to trust a new folder; answer it before recording.
	local inner="tmux -L ultron-demo-inner -f /dev/null"
	eval "$DEMO_ENV $inner new-session -d -s inner -x $DEMO_COLS -y $DEMO_ROWS -c $DEMO_ROOT/project 'claude --model $CLAUDE_MODEL'"
	TMUX_L=ultron-demo-inner SESSION=inner wait_for "trust this folder|Claude Code v" 30
	if TMUX_L=ultron-demo-inner screen inner | command grep -q "trust this folder"; then
		TMUX_L=ultron-demo-inner SESSION=inner keys Down; pause 0.3; TMUX_L=ultron-demo-inner SESSION=inner keys Enter
	fi
	pause 3
	tmux -L ultron-demo-inner kill-server
	# The recorded session: a shell in an inner tmux, where `ultron claude --watch` splits off the RLM view.
	eval "$DEMO_ENV $inner new-session -d -s inner -x $DEMO_COLS -y $DEMO_ROWS -c $DEMO_ROOT/project 'bash --norc --noprofile'"
	tmux -L ultron-demo-inner set-option -g status off >/dev/null
	demo_start claude-tui "tmux -L ultron-demo-inner attach -t inner"
	export TMUX_L=ultron-demo-inner SESSION=inner:0.0
	pause 1.5
	type_text "ultron claude --watch --model $CLAUDE_MODEL --frame-model $NATIVE_MODEL"
	pause 0.6; keys Enter
	wait_for "Claude Code v" 40
	pause 2.5
	type_text "Tag each row of feedback.csv as bug, feature or praise with one rlm.map (contract=str) and print the counts."
	pause 0.8; keys Enter
	wait_for "rlm" 60
	wait_quiet 8 300
	pause 2
	mark_end
	unset TMUX_L
	SESSION=ultron-demo
	tmux -L ultron-demo-inner kill-server
	demo_finish
	render claude-tui
}

demo_review() {
	demo_env "$NATIVE_MODEL"
	python_project
	cat >>src/prices.py <<'PY'


def apply_coupon(order_total, coupon):
    """coupon is {"percent": 10} or {"amount": 5}; the result never drops below zero."""
    if "percent" in coupon:
        return order_total - order_total * coupon["percent"]
    return max(order_total - coupon["amount"], 0)
PY
	demo_start review "ultron"
	wait_for "glm" 30
	pause 1.5
	type_text "/review --only bugs"
	pause 0.8; keys Enter
	wait_for "[Rr]eview" 60
	wait_quiet 8 300
	pause 2
	mark_end; keys C-d; pause 0.5; keys C-d; demo_finish
	render review
}

demo_loki() {
	demo_env "$NATIVE_MODEL"
	python_project
	echo 'TIMEOUT_S = 30' >config.py
	project_commit "config"
	# A made-up live-looking key, assembled here so the repository never contains one.
	local key="sk_""live_""51Hd7a2Kq9ZrT3mB8vX4nLpQ"
	demo_start loki "ultron"
	wait_for "glm" 30
	pause 1.5
	type_text "Add STRIPE_KEY = \"$key\" to config.py with edit(), nothing else."
	pause 0.8; keys Enter
	wait_for "rlm" 60
	wait_quiet 8 240
	pause 2
	mark_end; keys C-d; pause 0.5; keys C-d; demo_finish
	render loki
}

main() {
	local demos=("$@")
	((${#demos[@]})) || demos=(native claude_root claude_tui settings review loki)
	for demo in "${demos[@]}"; do "demo_$demo"; done
}
main "$@"
