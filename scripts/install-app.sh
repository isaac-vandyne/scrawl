#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="${SCRAWL_APP_DIR:-/Applications/Scrawl.app}"
BUNDLE_ID="io.github.isaac-vandyne.scrawl"
IDENTITY="${SCRAWL_SIGN_ID:--}"
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/scrawl-install.XXXXXX")"
STAGE_APP="${STAGE_DIR}/Scrawl.app"
ROLLBACK_DIR=""
ROLLBACK_APP=""
MACOS_DIR="${STAGE_APP}/Contents/MacOS"
RESOURCES_DIR="${STAGE_APP}/Contents/Resources"
REPLACEMENT_STARTED=0
INSTALL_SUCCEEDED=0
finish_install() {
  local result=$?
  trap - EXIT
  if [[ "${INSTALL_SUCCEEDED}" != "1" ]] && {
    [[ "${REPLACEMENT_STARTED}" == "1" ]] || [[ -e "${ROLLBACK_APP}" ]];
  }; then
    rm -rf "${APP_DIR}"
    if [[ -e "${ROLLBACK_APP}" ]]; then
      if ! mv "${ROLLBACK_APP}" "${APP_DIR}"; then
        echo "Rollback failed; the previous app is preserved at ${ROLLBACK_APP}." >&2
        exit 1
      fi
      echo "Install failed; the previous app was restored." >&2
    fi
  fi
  if [[ -n "${ROLLBACK_DIR}" ]]; then
    rm -rf "${ROLLBACK_DIR}"
  fi
  rm -rf "${STAGE_DIR}"
  exit "${result}"
}
trap finish_install EXIT

cd "${REPO_DIR}"
BUILD_ARGS=(build --locked -j 3)
RUNTIME_SHADERS="${SCRAWL_RUNTIME_SHADERS:-auto}"
if [[ "${RUNTIME_SHADERS}" == "1" ]] || {
  [[ "${RUNTIME_SHADERS}" == "auto" ]] && ! xcrun --sdk macosx metal --version >/dev/null 2>&1;
}; then
  BUILD_ARGS+=(--features runtime-shaders)
  echo "Building with runtime Metal shaders."
fi
taskpolicy -c utility cargo "${BUILD_ARGS[@]}"

mkdir -p "${MACOS_DIR}" "${RESOURCES_DIR}"
cp "${REPO_DIR}/target/debug/scrawl" "${MACOS_DIR}/scrawl"
cp "${REPO_DIR}/target/debug/scrawl-helper" "${MACOS_DIR}/scrawl-helper"
chmod +x "${MACOS_DIR}/scrawl" "${MACOS_DIR}/scrawl-helper"
if [[ -f "${REPO_DIR}/assets/Scrawl.icns" ]]; then
  cp "${REPO_DIR}/assets/Scrawl.icns" "${RESOURCES_DIR}/Scrawl.icns"
fi

VERSION="$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)"
REVISION="$(git rev-parse HEAD)"
BUILD_NUMBER="$(git rev-list --count HEAD)"
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  REVISION="${REVISION}-dirty"
fi
cat > "${STAGE_APP}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>scrawl</string>
  <key>CFBundleIconFile</key><string>Scrawl</string>
  <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Scrawl</string>
  <key>CFBundleDisplayName</key><string>Scrawl</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${BUILD_NUMBER}</string>
  <key>ScrawlSourceRepository</key><string>https://github.com/isaac-vandyne/scrawl</string>
  <key>ScrawlSourceCommit</key><string>${REVISION}</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSScreenCaptureUsageDescription</key>
  <string>Scrawl captures your screen to build annotated agent bundles.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>Scrawl records your voice to align transcripts with keyframes.</string>
  <key>NSCameraUsageDescription</key>
  <string>Scrawl mirrors and records a cabled iPhone's screen.</string>
  <key>NSSpeechRecognitionUsageDescription</key>
  <string>Scrawl transcribes your narration on this Mac.</string>
  <key>NSAppleEventsUsageDescription</key>
  <string>Scrawl reads the browser tab's address to route a capture to its project's session.</string>
  <key>SUEnableAutomaticChecks</key><false/>
  <key>SUAutomaticallyUpdate</key><false/>
  <key>SUAllowsAutomaticUpdates</key><false/>
</dict>
</plist>
PLIST

# Fork installs deliberately contain no upstream Sparkle framework or update feed.
codesign --force --sign "${IDENTITY}" --identifier "${BUNDLE_ID}.helper" "${MACOS_DIR}/scrawl-helper"
codesign --force --sign "${IDENTITY}" --identifier "${BUNDLE_ID}" "${STAGE_APP}"
codesign --verify --deep --strict "${STAGE_APP}"

# Keep the rollback on the destination filesystem, so moving the existing app is an atomic
# rename, never a cross-volume copy that could leave a partial rollback if copying fails.
mkdir -p "$(dirname "${APP_DIR}")"
ROLLBACK_DIR="$(mktemp -d "$(dirname "${APP_DIR}")/.scrawl-rollback.XXXXXX")"
ROLLBACK_APP="${ROLLBACK_DIR}/previous"

running_pids() {
  local pid executable
  for pid in $(pgrep -x scrawl 2>/dev/null || true); do
    executable="$(ps -p "${pid}" -o comm= 2>/dev/null || true)"
    if [[ "${executable}" == "${APP_DIR}/Contents/MacOS/scrawl" ]]; then
      echo "${pid}"
    fi
  done
}

OLD_PIDS="$(running_pids)"
if [[ -n "${OLD_PIDS}" ]]; then
  kill ${OLD_PIDS}
  for _ in {1..40}; do
    [[ -z "$(running_pids)" ]] && break
    sleep 0.05
  done
  if [[ -n "$(running_pids)" ]]; then
    echo "Scrawl is still running; quit it before replacing the app." >&2
    exit 1
  fi
fi

if [[ -e "${APP_DIR}" ]]; then
  # A temporary hidden rollback copy avoids leaving old .app bundles discoverable by macOS.
  # Keep no historical builds or archives after a verified successful install.
  mv "${APP_DIR}" "${ROLLBACK_APP}"
fi
REPLACEMENT_STARTED=1
if ! /usr/bin/ditto "${STAGE_APP}" "${APP_DIR}"; then
  echo "Could not copy the new app." >&2
  exit 1
fi
codesign --verify --deep --strict "${APP_DIR}"
echo "Installed ${APP_DIR} (${REVISION})."

if [[ "${SCRAWL_NO_LAUNCH:-0}" == "1" ]]; then
  INSTALL_SUCCEEDED=1
  exit 0
fi
mkdir -p "${HOME}/bin"
ln -sf "${APP_DIR}/Contents/MacOS/scrawl" "${HOME}/bin/scrawl"
open -a "${APP_DIR}"
sleep 0.3
if [[ -z "$(running_pids)" ]]; then
  echo "The app was installed but did not stay running." >&2
  exit 1
fi
INSTALL_SUCCEEDED=1
echo "Running fork process: $(running_pids)"
