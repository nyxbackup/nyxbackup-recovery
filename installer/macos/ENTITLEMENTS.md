# Hardened Runtime entitlements - Nyx Backup Recovery

`entitlements.plist` deliberately contains **no XML comments**.

Apple's AMFI parser rejects them outright:

    Failed to parse entitlements: AMFIUnserializeXML: syntax error near line 6

and `codesign` then signs the binary with **no entitlements at all** - or the
build fails outright, which is what happened on 2026-08-19 and produced a
build that emitted no package while still exiting 0.

So the rationale lives here instead.

## Why each entry exists

- `com.apple.security.cs.allow-jit`
- `com.apple.security.cs.allow-unsigned-executable-memory`
- `com.apple.security.cs.disable-library-validation`

These three are the documented minimum for a Tauri 2 app under the Hardened
Runtime: WKWebView JITs JavaScript, and the webview loads frameworks that are
not signed by us.  Removing any of them makes the app crash on launch under
notarization, not at build time - so the failure would only show up on a
customer's machine.

## Rule

Never add a comment to `entitlements.plist`.  Put it here.  The build script
greps for `<!--` and fails if one reappears.
