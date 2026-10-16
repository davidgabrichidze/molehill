#!/bin/sh
# Removes Molehill: sudo "/opt/molehill/uninstall.sh"
# Profiles in ~/Library/Application Support/Molehill and saved passwords in
# the Keychain stay; delete those by hand if you want them gone.
set -u
[ "$(id -u)" = 0 ] || { echo "Run it with sudo: sudo $0" >&2; exit 1; }

for uid in $(ps -axo uid=,comm= | awk '/loginwindow$/ {print $1}' | sort -u); do
    launchctl bootout "gui/$uid/io.github.giorgobiani.molehill" 2>/dev/null || true
done
pkill -x molehill 2>/dev/null || true
# Stopping the service closes the tunnels and their DNS settings; the sweep
# removes anything a crash left behind.
launchctl bootout system/io.github.giorgobiani.molehill.service 2>/dev/null || true
/opt/molehill/bin/molehill-service sweep 2>/dev/null || true

rm -rf /Applications/Molehill.app /opt/molehill \
    "/Library/Application Support/Molehill" /Library/Logs/Molehill \
    /Library/LaunchDaemons/io.github.giorgobiani.molehill.service.plist \
    /Library/LaunchAgents/io.github.giorgobiani.molehill.plist \
    /var/run/molehill.sock
pkgutil --forget io.github.giorgobiani.molehill >/dev/null 2>&1 || true
echo "Molehill removed."
