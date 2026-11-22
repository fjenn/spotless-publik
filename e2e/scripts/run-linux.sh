#!/usr/bin/env bash
# Linux E2E session: one D-Bus session bus (single-instance, MPRIS, the tray all live on it), a
# PulseAudio null sink as the sound card, Xvfb + openbox as the desktop (minimize/maximize need a
# window manager), a StatusNotifierWatcher for the tray, then WebdriverIO.
set -uo pipefail
cd "$(dirname "$0")/.."
export DISPLAY=:99
Xvfb :99 -screen 0 1600x1000x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
sleep 2
openbox >/tmp/openbox.log 2>&1 &
pulseaudio --start --exit-idle-time=-1 --daemonize=yes 2>/tmp/pulse.log || true
sleep 1
pactl load-module module-null-sink sink_name=e2e_speakers sink_properties=device.description=E2E_Speakers >/dev/null || true
pactl set-default-sink e2e_speakers || true
python3 scripts/sni-watcher.py >/tmp/sni.log 2>&1 &
sleep 1
echo "session bus: $DBUS_SESSION_BUS_ADDRESS"; pactl info 2>/dev/null | grep -E "Server Name|Default Sink" || true
npx wdio run wdio.conf.mjs
rc=$?
import -window root "${E2E_OUT:-out}/shots/zz-desktop.png" 2>/dev/null || true
exit $rc
