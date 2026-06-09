#!/usr/bin/env bash
# memd installer — downloads the latest prebuilt binary and runs `memd setup`.
#
#   curl -fsSL https://raw.githubusercontent.com/qdequele/memd/main/scripts/install.sh | bash
#
# Env overrides:
#   MEMD_VERSION   release tag to install (default: latest)
#   MEMD_NO_SETUP  set to "1" to skip running `memd setup`
set -euo pipefail

REPO="qdequele/memd"
INSTALL_DIR="${HOME}/.local/bin"
BIN="${INSTALL_DIR}/memd"

# --- detect platform ---------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "${os}-${arch}" in
  Darwin-arm64)   target="aarch64-apple-darwin" ;;
  Darwin-x86_64)  target="x86_64-apple-darwin" ;;
  Linux-x86_64)   target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64)  target="aarch64-unknown-linux-gnu" ;;
  *) echo "error: unsupported platform ${os}-${arch}" >&2; exit 1 ;;
esac

# --- resolve download url ----------------------------------------------------
asset="memd-${target}"
if [ "${MEMD_VERSION:-latest}" = "latest" ]; then
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
  url="https://github.com/${REPO}/releases/download/${MEMD_VERSION}/${asset}"
fi

echo "==> Downloading ${asset} (${MEMD_VERSION:-latest})…"
mkdir -p "${INSTALL_DIR}"
tmp="$(mktemp)"
if ! curl -fSL --proto '=https' "${url}" -o "${tmp}"; then
  echo "error: download failed from ${url}" >&2
  echo "       (no release yet? build from source: git clone + ./install.sh)" >&2
  exit 1
fi
install -m 0755 "${tmp}" "${BIN}"
rm -f "${tmp}"
echo "==> Installed ${BIN}"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "    (add ${INSTALL_DIR} to your PATH to run 'memd' directly)" ;;
esac

# --- run setup ---------------------------------------------------------------
if [ "${MEMD_NO_SETUP:-0}" = "1" ]; then
  echo "==> Skipping setup (MEMD_NO_SETUP=1). Run '${BIN} setup' when ready."
else
  echo "==> Running 'memd setup'…"
  exec "${BIN}" setup
fi
