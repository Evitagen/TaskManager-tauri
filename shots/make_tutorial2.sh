#!/bin/bash
# make_tutorial2.sh — produces docs/tutorial.mp4 from the app's OWN verify
# mode (TM_VERIFY=1): the app drives its real UI via JS (tab tour + per-core
# grid + end-task confirm on a spawned `sleep` child) on live system data.
# We only record: segmented x11grab that follows the window if it moves,
# log-timed captions, intro card, outro fade. No synthetic mouse needed.
set -u
BASE=/home/tn/Desktop/DeepseekHarnes/Taskmanager/tauri
REC=$BASE/shots/rec
LOG=$REC/verify.log
TIMES=$REC/times.txt
mkdir -p "$REC"
export DISPLAY=:0
XG=$BASE/shots/xg

FONT=$(fc-list 2>/dev/null | grep -i "dejavusans\.ttf" | head -1 | cut -d: -f1)
[ -n "$FONT" ] && [ -f "$FONT" ] || { echo "FAIL: no DejaVuSans font"; exit 1; }
if [ ! -x "$XG" ]; then
  [ -f "$BASE/shots/xg.c" ] || git -C "$BASE" checkout -- shots/xg.c
  gcc -O2 -o "$XG" "$BASE/shots/xg.c" -lX11 || exit 1
fi

# stale instances ignore SIGTERM here — hard-kill for a clean slate
for p in $(pgrep -f "target/debug/task-manager" 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
sleep 1

: > "$TIMES"
TM_VERIFY=1 TM_NO_JIGGLE=1 "$BASE/target/debug/task-manager" >/dev/null 2>"$LOG" &
APPPID=$!
FFPID=""
cleanup() {
  [ -n "${FFPID:-}" ] && kill -9 "$FFPID" 2>/dev/null
  [ -n "$APPPID" ] && kill -9 "$APPPID" 2>/dev/null
  [ -n "$APPPID" ] && wait "$APPPID" 2>/dev/null
}
trap cleanup EXIT

echo "warming up (100 s)…"
STARTED=0
for i in $(seq 1 200); do
  if grep -q "DUMP overview" "$LOG" 2>/dev/null; then STARTED=1; break; fi
  kill -0 "$APPPID" 2>/dev/null || break
  sleep 1
done
[ "$STARTED" = 1 ] || { echo "FAIL: tour did not start"; cat "$LOG" | head; exit 1; }

# find the verify window (unique title; retry — WebKit is sometimes slow)
TID=""
for i in $(seq 1 40); do
  for wid in $(xprop -root _NET_CLIENT_LIST 2>/dev/null | grep -o '0x[0-9a-fA-F]*'); do
    n=$(xprop -id "$wid" _NET_WM_NAME 2>/dev/null)
    case "$n" in *"Tauri Verify"*) TID=$wid; break 2 ;; esac
  done
  sleep 1
done
[ -n "$TID" ] || { echo "FAIL: no verify window"; exit 1; }
WID_DEC=$((TID))
echo "window $TID (id $WID_DEC)"

# ── single continuous capture straight from the window's own pixmap ─────
# x11grab -window_id: position/overlap independent, no compositor staleness
# (the app renders straight to its X window). Timestamps stay monotonic.
T_START=$(date +%s.%N)
ffmpeg -hide_banner -loglevel error -f x11grab -window_id "$WID_DEC" \
  -video_size 1240x800 -framerate 30 \
  -i :0.0 \
  -c:v libx264 -preset veryfast -crf 22 -pix_fmt yuv420p -an \
  "$REC/raw.mp4" &
FFPID=$!
sleep 1.5

# follow the log for tab markers + completion (polling — no tail orphan)
LINES=0
while :; do
  if grep -q "KILLTEST PASS\|KILLTEST FAIL\|\[verify\] done" "$LOG" 2>/dev/null; then sleep 1.5; break; fi
  kill -0 "$APPPID" 2>/dev/null || break
  N=$(wc -l < "$LOG" 2>/dev/null || echo 0)
  if [ "$N" -gt "$LINES" ]; then
    while IFS= read -r line; do
      case "$line" in
        *"DUMP "*)
          rest=${line#*DUMP }; echo "${rest%% *} $(date +%s.%N)" >> "$TIMES" ;;
        *"KILLTEST "*)
          echo "kill $(date +%s.%N)" >> "$TIMES" ;;
      esac
    done < <(tail -n +$((LINES + 1)) "$LOG")
    LINES=$N
  fi
  sleep 0.5
done
kill -INT "$FFPID" 2>/dev/null
wait "$FFPID" 2>/dev/null
FFPID=""
kill -9 "$APPPID" 2>/dev/null
APPPID=""
wait "$APPPID" 2>/dev/null
echo "tour timing:"
cat "$TIMES"

DUR=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$REC/raw.mp4")
echo "raw: ${DUR}s"

# ── captions from measured tab times ────────────────────────────────────
python3 - "$T_START" "$DUR" "$TIMES" "$REC/captions.txt" <<'PYEOF'
import sys
t0, dur, timesf, out = float(sys.argv[1]), float(sys.argv[2]), sys.argv[3], sys.argv[4]
labels = {
    "overview": "1  Overview — the whole system at a glance",
    "cpu": "2  CPU — right-click the graph for per-core detail",
    "mem": "3  Memory",
    "gpu": "4  GPU — live telemetry per adapter",
    "disk": "5  Disks — activity, speed and space",
    "net": "6  Network — every physical interface",
    "tasks": "7  Running tasks — select a process, End task",
}
times = {}
for line in open(timesf):
    parts = line.split()
    if len(parts) == 2:
        times[parts[0]] = float(parts[1])
def rel(t):
    return max(0.0, t - t0)
seq = [k for k in ["overview", "cpu", "mem", "gpu", "disk", "net", "tasks"] if k in times]
lines = []
for i, k in enumerate(seq):
    start = rel(times[k]) - 2.0   # tab was clicked ~2 s before its DUMP line
    if i + 1 < len(seq):
        end = rel(times[seq[i + 1]]) - 2.0
    elif "kill" in times:
        end = rel(times["kill"])
    else:
        end = dur - 0.6
    if "kill" in times and k == "tasks":
        # keep the tasks caption until the kill confirm, drop the "kill" label
        pass
    lines.append(f"{max(0.2, start + 0.3):.2f} {min(end, dur - 0.4):.2f} {labels[k]}")
open(out, "w").write("\n".join(lines) + "\n")
print(open(out).read())
PYEOF

# ── intro card + final encode ────────────────────────────────────────────
W=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$REC/raw.mp4" | cut -d, -f1)
H=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$REC/raw.mp4" | cut -d, -f2)
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "color=c=0x14151a:s=${W}x${H}:r=30:d=3" \
  -vf "drawtext=fontfile=$FONT:text='Task Manager':fontcolor=0xe8e8ec:fontsize=64:x=(w-text_w)/2:y=(h/2)-70,drawtext=fontfile=$FONT:text='A quick tour':fontcolor=0x9aa0aa:fontsize=28:x=(w-text_w)/2:y=(h/2)+15,fade=t=in:st=0:d=0.4,fade=t=out:st=2.6:d=0.4" \
  -c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p "$REC/intro.mp4"

VF=""
while IFS=' ' read -r a b rest; do
  [ -n "$a" ] || continue
  VF="${VF}drawtext=fontfile=$FONT:text='$rest':fontcolor=0xe8e8ec:fontsize=18:x=(w-text_w)/2:y=h-44:box=1:boxcolor=0x000000@0.55:boxborderw=9:enable='between(t,$a,$b)',"
done < "$REC/captions.txt"
VF="${VF}fade=t=out:st=${DUR%%.*}:d=0.6"

mkdir -p "$BASE/docs"
ffmpeg -hide_banner -loglevel error -y -i "$REC/intro.mp4" -i "$REC/raw.mp4" \
  -filter_complex "[1:v]${VF}[v1];[0:v][v1]concat=n=2:v=1[a]" \
  -map "[a]" -c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p \
  "$BASE/docs/tutorial.mp4"

rm -f "$REC"/seg_*.mp4 "$REC/raw.mp4" "$REC/intro.mp4" "$REC/concat.txt"
ls -la "$BASE/docs/tutorial.mp4"
ffprobe -v error -show_entries format=duration -of csv=p=0 "$BASE/docs/tutorial.mp4"
echo "TUTORIAL OK"
