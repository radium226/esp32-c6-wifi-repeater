#!/usr/bin/env bash
# Move the laptop onto the ESP32 bridge AP, test it, and always switch back.
# Run detached: setsid nohup ./laptop-test.sh >/dev/null 2>&1 </dev/null &
# Results land in test-logs/laptop_test.log and test-logs/esp_during_test.log.
# Needs STA_SSID, AP_SSID and AP_PASS in the environment: run it through
# `mise run laptop-test`, which loads them from mise.local.toml.
set -u
: "${STA_SSID:?run through mise}" "${AP_SSID:?run through mise}" "${AP_PASS:?run through mise}"
# The laptop's Wi-Fi interface (wpa_supplicant-managed) and its saved home network.
IF=${WIFI_INTERFACE:-$(iw dev | awk '/Interface/{print $2; exit}')}
HOME_ID=$(wpa_cli -i "$IF" list_networks | awk -F'\t' -v ssid="$STA_SSID" '$2 == ssid {print $1; exit}')
DIR=$(cd "$(dirname "$0")" && pwd)
OUT=$DIR/test-logs
mkdir -p "$OUT"
exec >"$OUT/laptop_test.log" 2>&1

log() { echo "[$(date +%T)] $*"; }
NID=""
revert() {
  log "REVERT: back to $STA_SSID"
  wpa_cli -i $IF select_network $HOME_ID >/dev/null
  [ -n "$NID" ] && wpa_cli -i $IF remove_network "$NID" >/dev/null
  for i in $(seq 1 30); do
    sleep 1
    if ping -c1 -W1 1.1.1.1 >/dev/null 2>&1; then log "home network back, internet OK after ${i}s"; break; fi
  done
  wpa_cli -i $IF status | grep -E '^(ssid|ip_address)='
  timeout 3 stty -F /dev/ttyACM0 hupcl 2>/dev/null
  [ -n "${PS_BEFORE:-}" ] && sudo -n iw dev $IF set power_save "$PS_BEFORE"
  log "power save: $(iw dev $IF get power_save)"
  [ -n "${CAT_PID:-}" ] && kill "$CAT_PID" 2>/dev/null
  log "DONE"
}
trap revert EXIT

[ -n "$HOME_ID" ] || { echo "no saved wpa_supplicant network for $STA_SSID"; exit 1; }

# Read the ESP log without touching DTR/RTS (espflash monitor parks it in the bootloader).
timeout 3 stty -F /dev/ttyACM0 raw -echo -hupcl 115200 || echo "serial port hung, no ESP log"
timeout 150 cat /dev/ttyACM0 > "$OUT/esp_during_test.log" 2>&1 &
CAT_PID=$!

# Client power save makes the AP buffer downlink frames until the next beacon.
PS_BEFORE=$(iw dev $IF get power_save | awk '{print $3}')
NO_PS=${NO_PS:-1}
[ "$NO_PS" = 1 ] && sudo -n iw dev $IF set power_save off
log "power save: $(iw dev $IF get power_save)"

NID=$(wpa_cli -i $IF add_network | tail -1)
wpa_cli -i $IF set_network "$NID" ssid "\"$AP_SSID\"" >/dev/null
wpa_cli -i $IF set_network "$NID" psk "\"$AP_PASS\"" >/dev/null
wpa_cli -i $IF set_network "$NID" key_mgmt WPA-PSK >/dev/null
unset AP_PASS
log "switching to $AP_SSID (network $NID)"
wpa_cli -i $IF select_network "$NID" >/dev/null

ok=""
for i in $(seq 1 45); do
  sleep 1
  st=$(wpa_cli -i $IF status)
  if echo "$st" | grep -q "wpa_state=COMPLETED" && echo "$st" | grep -q "ssid=$AP_SSID" \
     && ip -4 addr show $IF | grep -q "inet "; then ok=1; log "associated + IPv4 after ${i}s"; break; fi
done
wpa_cli -i $IF status | grep -E '^(bssid|ssid|wpa_state|ip_address)='
[ -z "$ok" ] && { log "FAIL: no association/lease in 45s"; exit 1; }

iw dev $IF link | grep -E 'signal|bitrate'
[ "${SPEED_ONLY:-0}" = 1 ] && SKIP_BASIC=1
GW=$(ip route | awk '/default/{print $3; exit}')
if [ -z "${SKIP_BASIC:-}" ]; then
log "ping gateway $GW";  ping -c10 -i0.3 -W2 "$GW" | tail -2
ip neigh show "$GW"
log "ping 1.1.1.1";      ping -c10 -i0.3 -W2 1.1.1.1 | tail -2 || { log "FAIL: no internet"; exit 1; }
log "HTTPS";             curl -s -o /dev/null -m 10 -w 'http %{http_code} in %{time_total}s\n' https://example.com || { log "FAIL: no HTTPS"; exit 1; }
fi
if [ "${DEBUG_LOCAL:-0}" = 1 ]; then
  ping -c3 -W1 192.168.4.1 | tail -2; ip neigh show dev $IF
  for i in 1 2 3 4 5 6; do sleep 2; curl -s -m 20 -o /dev/null -w "api: HTTP %{http_code} %{size_download} B in %{time_total}s\n" http://192.168.4.1/api/status; done
  ip neigh show dev $IF; exit 0
fi
log "status page via repeater.local"
getent hosts repeater.local
curl -s -m 5 -o /dev/null -w 'page: HTTP %{http_code}, %{size_download} B in %{time_total}s\n' http://repeater.local/
curl -s -m 5 http://repeater.local/api/status | python3 -I -c 'import sys,json; d=json.load(sys.stdin); print("api:", d["ip"], "associated clients", d["associated_clients"], "client addresses", d["client_addresses"])'
# Time-capped, so a slow bridge still reports a speed.
log "download (20 s max)"; curl -s -o /dev/null -m 20 -w 'down %{speed_download} B/s (%{size_download} B)\n' 'https://speed.cloudflare.com/__down?bytes=50000000'
log "upload (20 s max)";   head -c 20000000 /dev/zero | curl -s -o /dev/null -m 20 -w 'up %{speed_upload} B/s (%{size_upload} B)\n' --data-binary @- https://speed.cloudflare.com/__up
iw dev $IF link | grep -E 'signal|bitrate'
log "tests finished"
