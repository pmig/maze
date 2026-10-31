# shellcheck shell=bash
# Shared by script/bundle-mac (the release Maze.app) and script/dev-bundle (the
# Maze Dev.app that `mise start` runs): the release channel, bundle ID, app name and
# Info.plist of a macOS app bundle. Source it from the repository root.

# Prints the release channel of the build: MAZEMAIL_BUILD_CHANNEL (test-only), else
# the file RELEASE_CHANNEL_FILE (default crates/mazemail/RELEASE_CHANNEL); missing
# means dev. Like crates/release_channel, which the binary is built with.
mac_release_channel() {
  local file=${RELEASE_CHANNEL_FILE:-crates/mazemail/RELEASE_CHANNEL}
  if [ -n "${MAZEMAIL_BUILD_CHANNEL:-}" ]; then
    printf '%s\n' "$MAZEMAIL_BUILD_CHANNEL"
  elif [ -f "$file" ]; then
    tr -d '[:space:]' < "$file"
    echo
  else
    echo dev
  fi
}

# Prints the bundle ID of a channel (ReleaseChannel::bundle_id); fails for an unknown
# channel.
mac_bundle_id() {
  case $1 in
    dev) echo dev.mazemail.maze-dev ;;
    preview | stable) echo dev.mazemail.maze ;;
    *) echo "error: unknown release channel '$1' (in ${RELEASE_CHANNEL_FILE:-crates/mazemail/RELEASE_CHANNEL})" >&2; return 1 ;;
  esac
}

# Prints the app name of a channel (ReleaseChannel::app_name).
mac_app_name() {
  case $1 in
    dev) echo "Maze Dev" ;;
    *) echo Maze ;;
  esac
}

# Prints an Info.plist for the executable `mazemail`.
#   mac_info_plist <bundle id> <name> <version> <build number> <icon name or ""> <mailto: 0|1>
# <version> is Maze's semver (MazeVersion); CFBundleShortVersionString gets its
# numeric part. With mailto=1 the bundle registers as a mailto: handler.
mac_info_plist() {
  local bundle_id=$1 name=$2 version=$3 build_number=$4 icon=$5 mailto=$6
  local icon_plist="" url_types=""
  if [ -n "$icon" ]; then
    icon_plist="
  <key>CFBundleIconFile</key>
  <string>${icon}</string>"
  fi
  if [ "$mailto" = 1 ]; then
    url_types="
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key>
      <string>${bundle_id}.mailto</string>
      <key>CFBundleURLSchemes</key>
      <array>
        <string>mailto</string>
      </array>
    </dict>
  </array>"
  fi
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>mazemail</string>
  <key>CFBundleIdentifier</key>
  <string>${bundle_id}</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>${name}</string>
  <key>CFBundleDisplayName</key>
  <string>${name}</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${version%%-*}</string>
  <key>CFBundleVersion</key>
  <string>${build_number}</string>
  <key>MazeVersion</key>
  <string>${version}</string>${icon_plist}${url_types}
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.productivity</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
PLIST
}
