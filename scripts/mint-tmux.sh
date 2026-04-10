#!/usr/bin/env bash
# =============================================================================
# mint-tmux.sh — run a command inside a persistent tmux session on the
# Linux Mint build box so the user can attach and watch/interact at any time.
#
# Usage:
#   mint-tmux.sh run   <window-name> <command...>   # start a new tmux window
#   mint-tmux.sh send  <window-name> <text...>      # send keys to a window
#   mint-tmux.sh kill  <window-name>                # close a window
#   mint-tmux.sh list                               # list windows
#   mint-tmux.sh tail  <window-name>                # dump last 200 lines
#
# Session name is always "wzp". Attach manually with:
#   ssh -t root@172.16.81.192 tmux attach -t wzp
#
# If the wzp session doesn't exist yet, it's created automatically.
# =============================================================================
set -euo pipefail

HOST="root@172.16.81.192"
SESSION="wzp"
SSH_OPTS="-o ConnectTimeout=10 -o LogLevel=ERROR"

ensure_session() {
    ssh $SSH_OPTS "$HOST" "
        tmux has-session -t $SESSION 2>/dev/null || tmux new-session -d -s $SESSION -n home 'bash -l'
    "
}

cmd="${1:-list}"
shift || true

case "$cmd" in
    run)
        WIN="${1:?window name required}"; shift
        ensure_session
        # Use a heredoc so multi-arg commands don't need escaping
        CMD="$*"
        ssh $SSH_OPTS "$HOST" bash -s <<REMOTE
            if tmux list-windows -t $SESSION -F '#W' 2>/dev/null | grep -qx '$WIN'; then
                tmux kill-window -t $SESSION:$WIN 2>/dev/null || true
            fi
            tmux new-window -t $SESSION -n '$WIN' "bash -l -c '$CMD; echo; echo --- window $WIN exited with code \\\$?; exec bash -l'"
REMOTE
        echo "Started '$WIN' in tmux session $SESSION on $HOST"
        echo "Attach: ssh -t $HOST tmux attach -t $SESSION"
        ;;
    send)
        WIN="${1:?window name required}"; shift
        TEXT="$*"
        ssh $SSH_OPTS "$HOST" "tmux send-keys -t $SESSION:$WIN '$TEXT' C-m"
        ;;
    kill)
        WIN="${1:?window name required}"
        ssh $SSH_OPTS "$HOST" "tmux kill-window -t $SESSION:$WIN 2>/dev/null || true"
        ;;
    list)
        ensure_session
        ssh $SSH_OPTS "$HOST" "tmux list-windows -t $SESSION"
        ;;
    tail)
        WIN="${1:?window name required}"
        ssh $SSH_OPTS "$HOST" "tmux capture-pane -p -t $SESSION:$WIN -S -200 || echo 'no such window'"
        ;;
    attach)
        exec ssh -t $SSH_OPTS "$HOST" tmux attach -t $SESSION
        ;;
    *)
        sed -n '3,20p' "$0"
        exit 1
        ;;
esac
