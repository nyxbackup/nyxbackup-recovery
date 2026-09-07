#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
#
# Mint the Recovery Tool's icon as a phoenix-rising-from-flames glyph:
# three flame tips at the base, a stylised bird body with spread wings
# rising above, all in warm gold on a deep fire-brick red rounded
# square.  Reads as "recovery from disaster" even at 16x16 (Dock,
# Spotlight) - the wing arc + flame triangles silhouette is the part
# the eye picks up first.
#
# The drawing is hand-built via NSBezierPath in Swift (Xcode CLI tools
# only - no librsvg, ImageMagick, or external assets needed) and
# rendered at 1024x1024.  sips + iconutil downscale and assemble the
# .icns.  The 192x192 in-app logo.png is written too so the Recovery
# Tool's About page header matches the Dock icon.
#
# Output:
#   crates/bkp-recover/icons/icon.icns         (.app bundle icon)
#   crates/bkp-recover/ui/src/lib/logo.png     (in-app About + chrome)
#
# Idempotent.  Re-running overwrites both files.

set -euo pipefail

WORKSPACE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${WORKSPACE_DIR}/crates/bkp-recover/icons"
OUT_ICNS="${OUT_DIR}/icon.icns"
UI_LOGO="${WORKSPACE_DIR}/crates/bkp-recover/ui/src/lib/logo.png"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

if [[ "$(uname)" != "Darwin" ]]; then
    echo "ERROR: this generator uses Apple Swift (macOS only)." >&2
    exit 2
fi

mkdir -p "$OUT_DIR"

PHOENIX_1024="${TMP_DIR}/phoenix-1024.png"

/usr/bin/swift - "$PHOENIX_1024" <<'SWIFT'
import AppKit
import Foundation

let outPath = CommandLine.arguments[1]
let size: CGFloat = 1024

let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()

// Background: deep nautical-orange / international-orange rounded square.
// The 0.207 corner radius matches Apple's "squircle" macOS app-icon
// proportion, so the icon has the standard macOS app silhouette.
let bg = NSBezierPath(
    roundedRect: NSRect(x: 0, y: 0, width: size, height: size),
    xRadius: size * 0.207,
    yRadius: size * 0.207
)
NSColor(red: 0.82, green: 0.30, blue: 0.10, alpha: 1.0).setFill()
bg.fill()

// ── Life preserver / life ring ──────────────────────────────────────
// Classic ring buoy: alternating four white + four red bands around a
// hollow centre.  Universally recognised "rescue" symbol.  Drawn with
// concentric circles + four wedge masks rather than the SF Symbol so
// we control the band colours exactly (palette-matched to the
// background) and the silhouette stays crisp at 16x16.

let cx = size * 0.5
let cy = size * 0.5
let outerR = size * 0.36   // outer edge of the ring
let innerR = size * 0.16   // inner hole

// White ring base.
NSColor(red: 0.98, green: 0.97, blue: 0.94, alpha: 1.0).setFill()
let outerCircle = NSBezierPath(ovalIn: NSRect(
    x: cx - outerR, y: cy - outerR, width: outerR * 2, height: outerR * 2))
outerCircle.fill()

// Four red wedges at the diagonals (NE / SE / SW / NW).  Each is a
// 45-degree pie slice from cy/cx.  Using filled triangles that point
// from the centre outward and the area between innerR and outerR
// becomes the red band; the white ring shows through between the
// wedges.
NSColor(red: 0.78, green: 0.18, blue: 0.10, alpha: 1.0).setFill()
let wedgeAngles: [(CGFloat, CGFloat)] = [
    ( 22.5,  67.5),   // NE
    (112.5, 157.5),   // NW
    (202.5, 247.5),   // SW
    (292.5, 337.5),   // SE
]
for (startDeg, endDeg) in wedgeAngles {
    let wedge = NSBezierPath()
    wedge.move(to: NSPoint(x: cx, y: cy))
    wedge.appendArc(
        withCenter: NSPoint(x: cx, y: cy),
        radius: outerR,
        startAngle: startDeg,
        endAngle: endDeg,
        clockwise: false
    )
    wedge.close()
    wedge.fill()
}

// Punch out the inner hole.  destinationOut compositing mode makes
// the wedge red fill + the white ring both become transparent in the
// centre.  NSGraphicsContext is the AppKit compositing op anchor.
if let ctx = NSGraphicsContext.current {
    ctx.saveGraphicsState()
    ctx.compositingOperation = .destinationOut
    NSColor.black.setFill()  // colour irrelevant; alpha is what destinationOut consumes
    let innerHole = NSBezierPath(ovalIn: NSRect(
        x: cx - innerR, y: cy - innerR, width: innerR * 2, height: innerR * 2))
    innerHole.fill()
    ctx.restoreGraphicsState()
}

// Thin separator ring between the inner hole and the bands so the
// transition reads cleanly at small sizes.
NSColor(red: 0.78, green: 0.18, blue: 0.10, alpha: 1.0).set()
let separator = NSBezierPath(ovalIn: NSRect(
    x: cx - innerR - size * 0.005, y: cy - innerR - size * 0.005,
    width: (innerR + size * 0.005) * 2, height: (innerR + size * 0.005) * 2))
separator.lineWidth = size * 0.008
separator.stroke()

// Thin outer separator ring at the edge of the outer circle for the
// same readability reason.
let outerSep = NSBezierPath(ovalIn: NSRect(
    x: cx - outerR, y: cy - outerR, width: outerR * 2, height: outerR * 2))
outerSep.lineWidth = size * 0.012
outerSep.stroke()

img.unlockFocus()

// PNG encode + write.
guard let tiffData = img.tiffRepresentation,
      let rep = NSBitmapImageRep(data: tiffData),
      let pngData = rep.representation(using: .png, properties: [:])
else {
    FileHandle.standardError.write("PNG encode failed\n".data(using: .utf8)!)
    exit(1)
}
try pngData.write(to: URL(fileURLWithPath: outPath))
SWIFT

# Assemble .iconset at every macOS-supported size.
ICONSET="${TMP_DIR}/icon.iconset"
mkdir -p "$ICONSET"
sips -z 1024 1024 "$PHOENIX_1024" --out "${ICONSET}/icon_512x512@2x.png" >/dev/null
sips -z 512  512  "$PHOENIX_1024" --out "${ICONSET}/icon_512x512.png"   >/dev/null
sips -z 512  512  "$PHOENIX_1024" --out "${ICONSET}/icon_256x256@2x.png" >/dev/null
sips -z 256  256  "$PHOENIX_1024" --out "${ICONSET}/icon_256x256.png"   >/dev/null
sips -z 256  256  "$PHOENIX_1024" --out "${ICONSET}/icon_128x128@2x.png" >/dev/null
sips -z 128  128  "$PHOENIX_1024" --out "${ICONSET}/icon_128x128.png"   >/dev/null
sips -z 64   64   "$PHOENIX_1024" --out "${ICONSET}/icon_32x32@2x.png"   >/dev/null
sips -z 32   32   "$PHOENIX_1024" --out "${ICONSET}/icon_32x32.png"     >/dev/null
sips -z 32   32   "$PHOENIX_1024" --out "${ICONSET}/icon_16x16@2x.png"   >/dev/null
sips -z 16   16   "$PHOENIX_1024" --out "${ICONSET}/icon_16x16.png"     >/dev/null

iconutil -c icns "$ICONSET" -o "$OUT_ICNS"

# In-app logo for the Recovery About page + chrome.
sips -z 192 192 "$PHOENIX_1024" --out "$UI_LOGO" >/dev/null

echo "Recovery icon written to: $OUT_ICNS"
echo "  ($(du -h "$OUT_ICNS" | cut -f1))"
echo "Recovery in-app logo written to: $UI_LOGO"
echo "  ($(du -h "$UI_LOGO" | cut -f1))"
