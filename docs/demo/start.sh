# Sets up the split screen the demo GIF is recorded in. To re-record:
#   1. cargo build --release
#   2. put a Kanri Linux binary at docs/demo/work/kanri (dpkg-deb -x the .deb from
#      github.com/kanriapp/kanri/releases, then copy usr/bin/kanri)
#   3. from the repo root: vhs docs/demo/demo.tape
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cd "$here/work" && rm -rf out check kanri-patched
export PATH="$root/target/release:$PATH"
tmux -L untauri-demo kill-server 2>/dev/null
exec tmux -L untauri-demo -f "$here/tmux.conf" new-session -c "$here/work" \; \
  split-window -h -l 33% -c "$here/work" "watch -t -c -n 0.2 bash '$here/tree.sh'" \; select-pane -L
