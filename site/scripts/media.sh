#!/bin/zsh
# Rebuilds the landing page's video and image assets from the 0.5.9 tour footage.
# Run each encode through heavy.sh (one heavy job at a time on a 16 GB Mac):
#   ~/.local/share/godspeed-ops/heavy.sh site/scripts/media.sh [hero|tour|clips|stills|all]
# Inputs (not in the repo):
#   TOUR   the 61 s 16:9 tour (godspeed-tour-x.mp4, 1920x1080)
#   SCENES raw 2560x1600 window captures from the demo fixture (one mp4 per scene)
set -euo pipefail
TOUR=${TOUR:-/Volumes/Lexar/tmp/xpost-0.5.9/godspeed-tour-x.mp4}
SCENES=${SCENES:-/Volumes/Lexar/tmp/godspeed-video/scenes}
OUT=${OUT:-${0:A:h}/../public/media}
mkdir -p "$OUT"
FF=(ffmpeg -v error -y -nostdin -threads 4)

vp9() { "${FF[@]}" -i "$1" -c:v libvpx-vp9 -b:v 0 -crf "$3" -row-mt 1 -deadline good -cpu-used 2 -pix_fmt yuv420p -an "$2"; }
x264() { "${FF[@]}" -i "$1" -c:v libx264 -crf "$3" -preset slow -profile:v high -pix_fmt yuv420p -movflags +faststart -an "$2"; }
png2webp() { cwebp -quiet -q "${3:-82}" -m 6 "$1" -o "$2"; }
poster() { local t="${TMPDIR:-/tmp}/gs-poster.png"; "${FF[@]}" -ss "$2" -i "$1" -frames:v 1 "$t"; png2webp "$t" "$3"; rm -f "$t"; }

# cut <src> <start> <dur> <vf> <name> <crf-vp9> <crf-264>
cut() {
  local tmp="${TMPDIR:-/tmp}/gs-$5.mov"
  "${FF[@]}" -ss "$2" -t "$3" -i "$1" -vf "$4,fps=30" -c:v prores_ks -profile:v 1 -an "$tmp"
  vp9 "$tmp" "$OUT/$5.webm" "$6"
  x264 "$tmp" "$OUT/$5.mp4" "$7"
  rm -f "$tmp"
}

hero() {
  # Zoomies from Home, Conductor, an approval, .env to a server, usage meters (tour 9.4 to 29.1 s).
  cut "$TOUR" 9.4 19.7 "scale=1920:1080:flags=lanczos" hero 34 24
  poster "$TOUR" 9.42 "$OUT/hero-poster.webp"
}

tour() {
  # Full tour with sound, loaded only when someone presses play.
  cp "$TOUR" "$OUT/tour.mp4"
  poster "$TOUR" 6.9 "$OUT/tour-poster.webp"
}

clips() {
  # Raw captures are 2560x1600. Crops are x:y:w:h in those pixels.
  cut "$SCENES/mission.mp4" 0 3.8 "crop=2560:1600:0:0,scale=1280:800:flags=lanczos" mission 36 26
  # The inbox capture holds still, so it ships as an image only.
  local t="${TMPDIR:-/tmp}/gs-inbox.png"; "${FF[@]}" -sseof -0.2 -i "$SCENES/inbox.mp4" -frames:v 1 -vf "crop=1480:925:820:60,scale=1184:740:flags=lanczos" "$t"; png2webp "$t" "$OUT/inbox.webp" 84; rm -f "$t"
  cut "$SCENES/receipt.mp4" 0 3.56 "crop=2016:1300:544:60,scale=1116:720:flags=lanczos" receipt 36 26
  for n in mission receipt; do
    local d=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$OUT/$n.mp4")
    poster "$OUT/$n.mp4" "$(( d - 0.1 ))" "$OUT/$n-poster.webp"
  done
}

stills() {
  still() { local t="${TMPDIR:-/tmp}/gs-still.png"; "${FF[@]}" -sseof -0.3 -i "$SCENES/$1.mp4" -frames:v 1 -vf "crop=$2,scale=$3:-1:flags=lanczos" "$t"; png2webp "$t" "$OUT/$1.webp" 80; rm -f "$t"; }
  still conductor "1520:1240:790:80" 760
  still transfer  "1040:786:760:540" 720
  still usage     "1420:820:836:100" 760
}

case "${1:-all}" in
  hero) hero ;; tour) tour ;; clips) clips ;; stills) stills ;;
  all) hero; tour; clips; stills ;;
  *) echo "usage: $0 [hero|tour|clips|stills|all]" >&2; exit 2 ;;
esac
ls -la "$OUT"
