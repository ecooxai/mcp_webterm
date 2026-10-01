#!/usr/bin/env bash
# Webterm starts direct *.sh files in ~/project/autoboot after its 10s startup delay.
# Editing this file restarts it in the same named terminal. Deleting it removes that terminal.
set -euo pipefail

printf '[autoboot] template.sh: waiting 20 seconds before starting the demo app...\n'
sleep 20

# Change this to your application's existing directory.
APP_DIR="${HOME}/project"
cd -- "$APP_DIR"
printf '[autoboot] working directory: %s\n' "$PWD"

# Configure your app with environment variables, then replace the demo below.
# export PORT=3000
# exec npm run start -- --host 0.0.0.0
# exec node server.js
# exec python3 -u app.py
# exec ./my-app --port "$PORT"
# Keep apps in the foreground: use exec, not nohup, setsid, disown, or '&'.

# Harmless test app: prints a message, opens no port, and exits successfully.
exec python3 -u -c 'import os; print("[autoboot] demo app started successfully in " + os.getcwd())'
