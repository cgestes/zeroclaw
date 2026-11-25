#!/usr/bin/env bash
# Build and install zeroclaw from this checkout with the ctaf feature/app set.
# Uncomment a line to enable it. `./install.sh --list-features` shows the full list.
set -euo pipefail
cd "$(dirname "$0")"

# Added on top of the crate defaults: agent-runtime, default-channels
# (acp-server, webhook, email, telegram, discord, filesystem), acp-bridge,
# gateway, observability-prometheus, schema-export.
FEATURES=(
  # ── Channels ───────────────────────────────────────────────
  channel-slack
  channel-signal
  channel-git
  # channel-matrix
  # channel-mattermost
  # channel-irc
  # channel-twitch
  # channel-imessage
  # channel-whatsapp-cloud
  # whatsapp-web
  # channel-voice-call
  # channel-bluesky
  # channel-twitter
  # channel-reddit
  # channel-notion
  # channel-nostr
  # channel-mqtt
  # channel-amqp
  # channel-nextcloud
  # channel-linq
  # channel-line
  # channel-lark
  # channel-feishu
  # channel-dingtalk
  # channel-qq
  # channel-wechat
  # channel-wecom
  # channel-wecom-ws
  # channel-mochat
  # channel-clawdtalk
  # channels-full           # every channel above except matrix/nostr/wechat/whatsapp-web
  # voice-wake
  provider-github         # git channel: GitHub provider
  # provider-gitea          # git channel: Gitea provider

  # ── Tools ──────────────────────────────────────────────────
  tool-claude-code
  tool-codex-cli
  tool-google-workspace
  tools-external          # web_search
  # tool-claude-code-runner
  # tool-gemini-cli
  # tool-opencode-cli
  # tools-coding-cli        # all coding CLIs (claude-code, runner, codex, gemini, opencode)
  # tool-jira
  # tool-notion
  # tool-linkedin
  # tool-composio
  # tool-microsoft365
  # tool-project-intel
  # tools-saas              # jira, notion, linkedin, composio, google-workspace, ms365, project-intel
  # tools-compat            # tools-saas + tools-coding-cli + tools-external
  # browser-native

  # ── Plugins ────────────────────────────────────────────────
  plugins-wasm-cranelift  # WASM plugins (redact-text)
  # plugins-wasm
  # plugins-wasm-pulley
  # plugins-wasm-runtime-only

  # ── Platform / security ────────────────────────────────────
  # sandbox-landlock
  # sandbox-bubblewrap
  # webauthn
  # hardware
  # peripheral-rpi
  # probe
  # dev-sim

  # ── Storage / observability / UI ───────────────────────────
  # memory-postgres
  # observability-otel
  # embedded-web            # bundle the dashboard into the binary (needs Node/npm)
)

APPS=(
  zerocode          # terminal UI
  zerorelay         # relay server
  zeroclaw-desktop  # desktop app (apps/tauri)
)

INSTALL_ARGS=(
  --source
  --skip-quickstart
  --no-modify-path
  # --without-gateway        # drop the web gateway/dashboard
  # --with-gateway           # force the gateway on
  # --prefix "$HOME/zc-test" # isolated install root (CARGO_HOME, source, config)
  # --dry-run                # show what would happen, build nothing
)

exec ./install.sh "${INSTALL_ARGS[@]}" \
  --features "$(IFS=,; echo "${FEATURES[*]}")" \
  --apps "$(IFS=,; echo "${APPS[*]}")"
