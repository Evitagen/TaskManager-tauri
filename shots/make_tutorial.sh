#!/bin/bash
# make_tutorial.sh — records a guided tour of the task manager and produces
# docs/tutorial.mp4 (intro card + captioned tour + outro fade).
#
# Runs the app on a PRIVATE TigerVNC X server (:99, 1240x800): no desktop
# clutter, no user windows, nothing to move. Drives the UI with real
# pointer/keyboard events via shots/demo_mouse (XTEST, device 0) and
# captures with ffmpeg x11grab. GPU/CPU/mem/disk/net data is all real.
set -u
BASE=/home/tn/Desktop/DeepseekHarnes/Taskmanager/tauri
M=$BASE/shots/demo_mouse
XG=$BASE/shots/xg
REC=$BASE/shots/rec
mkdir -p "$REC"
export DISPLAY=:99

FONT=$(fc-list 2>/dev/null | grep -i "dejavusans\.ttf" | head -1 | cut -d: -f1)
[ -n "$FONT" ] && [ -f "$FONT" ] || { echo "FAIL: no DejaVuSans font"; exit 1; }

# ── private X server ────────────────────────────────────────────────────
XVNC_STARTED=0
if ! ls /tmp/.X11-unix/X99 >/dev/null 2>&1; then
  Xvnc :99 -geometry 1240x800x24 -ac -noreset -nolisten tcp >/dev/null 2>&1 &
  XVNC_STARTED=1
  for i in $(seq 1 30); do ls /tmp/.X11-unix/X99 >/dev/null 2>&1 && break; sleep 0.5; done
fi
ls /tmp/.X11-unix/X99 >/dev/null 2>&1 || { echo "FAIL: :99 did not start"; exit 1; }
echo "using X display :99 (started=$XVNC_STARTED)"

# ── (re)build helpers if missing — they do not always survive between runs
[ -x "$M" ] || gcc -O2 -o "$M" "$BASE/shots/demo_mouse.c" -lX11 -lXtst || exit 1
if [ ! -x "$XG" ]; then
  [ -f "$BASE/shots/xg.c" ] || git -C "$BASE" checkout -- shots/xg.c
  gcc -O2 -o "$XG" "$BASE/shots/xg.c" -lX11 || exit 1
fi

cleanup() {
  [ -n "$APPPID" ] && kill -9 "$APPPID" 2>/dev/null
  [ -n "$APPPID" ] && wait "$APPPID" 2>/dev/null
  [ "$XVNC_STARTED" = 1 ] && pkill -9 -f "Xvnc :99" 2>/dev/null
}
APPPID=""
trap cleanup EXIT

# ── start the app (plain mode, on :99) ─────────────────────────────────
# Tauri instances here ignore SIGTERM; hard-kill stale ones for a clean slate
for p in $(pgrep -f "target/debug/task-manager" 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
sleep 1
"$BASE/target/debug/task-manager" >/dev/null 2>&1 &
APPPID=$!
sleep 8   # webview + first tick on software rendering

TID=""
for tries in $(seq 1 20); do
  for wid in $(xprop -root _NET_CLIENT_LIST 2>/dev/null | grep -o '0x[0-9a-fA-F]*'); do
    pid=$(xprop -id "$wid" _NET_WM_PID 2>/dev/null | grep -o '[0-9]*$')
    if [ "$pid" = "$APPPID" ]; then TID=$wid; break 2; fi
  done
  sleep 1
done
[ -n "$TID" ] || { echo "FAIL: app window not found on :99"; exit 1; }
sleep 2   # let the first frame paint
read X Y W H < <($XG "$TID")
echo "app window $TID at $X,$Y ${W}x${H}"

# no WM on :99 — park the window at the top-left so it fills the capture
$M winmove "$TID" 0 0
sleep 0.5
read X Y W H < <($XG "$TID")
echo "window pinned at $X,$Y ${W}x${H}"

# pick a process that exists for the search demo
TARGET=$(pgrep -x pipewire >/dev/null 2>&1 && echo pipewire || echo systemd)
echo "search demo target: $TARGET"

# ── start capture ───────────────────────────────────────────────────────
ffmpeg -hide_banner -loglevel error -f x11grab -framerate 30 \
  -video_size "${W}x${H}" -i ":99.0+${X}+${Y}" \
  -c:v libx264 -preset veryfast -crf 22 -pix_fmt yuv420p -an \
  "$REC/raw.mp4" &
FFPID=$!
sleep 1   # let x11grab lock the display

C()  { $M click   $((X + $1)) $((Y + $2)); }
RC() { $M rclick  $((X + $1)) $((Y + $2)); }

# ── choreography (coords relative to the window's top-left) ─────────────
sleep 3
C 90 103;        sleep 3        # → CPU
RC 600 200;      sleep 1.4      # right-click the CPU graph → context menu
C 700 220;       sleep 3.8      # "Show logical cores" → 28-cell grid
C 90 138;        sleep 4        # → Memory
C 90 173;        sleep 4        # → GPU
C 90 208;        sleep 4        # → Disks
C 90 243;        sleep 4        # → Network
C 90 278;        sleep 2        # → Running tasks
C 1080 84;       sleep 0.6      # focus the search box
$M type "$TARGET"; sleep 2.2    # filter the list
C 300 188;       sleep 1.5      # select the first matching row
C 578 747;       sleep 2.5      # End task → confirm modal
C 150 400;       sleep 1.5      # click the scrim → cancel
C 90 68;         sleep 5        # → Overview

kill -INT "$FFPID" 2>/dev/null
wait "$FFPID" 2>/dev/null
kill -9 "$APPPID" 2>/dev/null
APPPID=""
wait 2>/dev/null
DUR=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$REC/raw.mp4")
echo "raw recording: ${DUR}s"

# ── post: intro card + captions + outro fade ────────────────────────────
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "color=c=0x14151a:s=${W}x${H}:r=30:d=3" \
  -vf "drawtext=fontfile=$FONT:text='Task Manager':fontcolor=0xe8e8ec:fontsize=64:x=(w-text_w)/2:y=(h/2)-70,drawtext=fontfile=$FONT:text='A quick tour':fontcolor=0x9aa0aa:fontsize=28:x=(w-text_w)/2:y=(h/2)+15,fade=t=in:st=0:d=0.4,fade=t=out:st=2.6:d=0.4" \
  -c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p "$REC/intro.mp4"

DT() { # DT <a> <b> <text>  -> drawtext enable window
  echo "drawtext=fontfile=$FONT:text='$3':fontcolor=0xe8e8ec:fontsize=18:x=(w-text_w)/2:y=h-44:box=1:boxcolor=0x000000@0.55:boxborderw=9:enable='between(t,$1,$2)',"
}
VF=$(cat <<EOF
$(DT 1.3  3.9  '1  Overview — the whole system at a glance')
$(DT 4.3  12.2 '2  CPU — right-click the graph for per-core detail')
$(DT 12.5 16.2 '3  Memory')
$(DT 16.5 20.2 '4  GPU — live telemetry per adapter')
$(DT 20.5 24.2 '5  Disks — activity, speed and space')
$(DT 24.5 28.2 '6  Network — every physical interface')
$(DT 28.5 38.2 '7  Running tasks — search, select, End task')
$(DT 38.7 42.0 'One window, no scrolling, ~200 MB of RAM')
fade=t=out:st=${DUR%%.*}:d=0.6
EOF
)
mkdir -p "$BASE/docs"
ffmpeg -hide_banner -loglevel error -y -i "$REC/intro.mp4" -i "$REC/raw.mp4" \
  -filter_complex "[1:v]${VF}[v1];[0:v][v1]concat=n=2:v=1[a]" \
  -map "[a]" -c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p \
  "$BASE/docs/tutorial.mp4"

rm -f "$REC/raw.mp4" "$REC/intro.mp4"
ls -la "$BASE/docs/tutorial.mp4"
ffprobe -v error -show_entries format=duration -of csv=p=0 "$BASE/docs/tutorial.mp4"
echo "TUTORIAL OK"
