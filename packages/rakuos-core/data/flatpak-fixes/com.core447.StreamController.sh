#!/usr/bin/env bash
set -euo pipefail

ACTION="${1:-unknown}"

if [[ "$ACTION" == "install" || "$ACTION" == "update" ]]; then

    echo "Applying StreamController fixes"
    wget https://raw.githubusercontent.com/StreamController/StreamController/main/udev.rules -O /etc/udev/rules.d/60-streamdeck.rules
    echo "Reload udev..."
    udevadm control --reload-rules && udevadm trigger
fi

if [[ "$ACTION" == "uninstall" ]]; then

    echo "Cleaning StreamController overrides"

    flatpak override --system --reset com.core447.StreamController || true

fi