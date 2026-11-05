# Contributing

Bug reports and ideas are welcome in [GitHub issues](https://github.com/dienp/overnight/issues), or through Report a Problem in the menu bar icon's menu, which fills in your macOS and app versions. [ARCHITECTURE.md](ARCHITECTURE.md) explains how the app works.

## Build it yourself

Overnight is built with Tauri 2: Rust underneath, with a plain HTML, CSS and JavaScript popover. You need Node and Rust. If Rust came from Homebrew's `rustup`, which is keg-only, the npm scripts add it to your `PATH`.

```bash
npm install
npm run dev        # run with hot reload
npm run build      # makes src-tauri/target/release/bundle/macos/Overnight.app
```

If linking fails with `tapi` errors about an unknown architecture, the Command Line Tools linker on your `PATH` and the SDK `xcode-select` points at disagree. Create `src-tauri/.cargo/config.toml` to use the Command Line Tools for both. Git ignores that file, so it stays on your machine.

```toml
[env]
DEVELOPER_DIR = "/Library/Developer/CommandLineTools"
SDKROOT = "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk"
```

## Tests and debugging

```bash
cd src-tauri && cargo test          # parsers, guards, summaries, phone alerts, the osascript launch command
sh scripts/test-helper.sh           # the root helper, with pmset and ioreg stubbed (no root needed)
src-tauri/target/debug/overnight --probe   # print live readings as JSON
```

To work on the popover without Tauri, serve `src/` (for example with `python3 -m http.server -d src`) and open `index.html`. It loads `preview-mock.js` in place of the backend. Query parameters choose the state: `state=off`, `on` or `standby`; `limit=waiting` or `missed`; `auto=off`; `ask=question` or `approval`; `phone=on`; `update=on`; and `theme=dark`.

`npm run icons` redraws the menu bar and app icons from `scripts/make-icons.py`.

## Releases

Every push to `main` that changes the app, whether a merged pull request or a direct push, makes a release through [the release workflow](.github/workflows/release.yml). It works out the next version with `scripts/release-version.mjs`, runs the tests, builds a universal app for Apple silicon and Intel, signs it and has Apple notarize it (see [Signing and notarization](#signing-and-notarization)), and publishes `Overnight-macos.zip` on the [Releases page](https://github.com/dienp/overnight/releases). It also publishes `Overnight.app.tar.gz` and `latest.json`, which the app's updater reads. Then it commits the new version to `main` and tags it. Releases run one at a time. If something lands on `main` while one builds, that run stops before publishing and the run queued behind it releases the newer code.

The updater archive is signed with the key in the `TAURI_SIGNING_PRIVATE_KEY` repository secret. The key has no password, so `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` stays unset. Installed copies only accept archives signed by that key, whose public half is in `src-tauri/tauri.conf.json`. Keep a backup of the private key. If it's lost, a new key needs a new public key in the app, and everyone has to download that version by hand once.

The version goes up by a patch number each time. Put `[minor]` or `[major]` in a commit message for a bigger step, or `[skip release]` to leave a push out. Changes that only touch `docs/` or Markdown files don't release. To see what the next version would be:

```bash
node scripts/release-version.mjs --dry-run
```

## Signing and notarization

Releases are signed with a Developer ID and notarized once five repository secrets are set. Without them the app is ad-hoc signed, as local builds are, and people have to click Open Anyway in Privacy & Security the first time they open it. Setting them up needs a paid Apple Developer Program membership.

1. Create a Developer ID Application certificate. In Xcode, open Settings › Accounts, choose your team, click Manage Certificates, then + › Developer ID Application.
2. In Keychain Access, find the certificate under My Certificates, Control-click it, choose Export and save it as a `.p12` file with a password.
3. Create an app-specific password for your Apple Account at [account.apple.com](https://account.apple.com), under Sign-In and Security › App-Specific Passwords. Notarization signs in with it.
4. Find your team ID under Membership details at [developer.apple.com/account](https://developer.apple.com/account).
5. From the repository folder, add the `.p12` file as `APPLE_CERTIFICATE`, its password as `APPLE_CERTIFICATE_PASSWORD`, your Apple Account email as `APPLE_ID`, the app-specific password as `APPLE_PASSWORD` and the team ID as `APPLE_TEAM_ID`. Each command after the first asks for its value.

```bash
base64 -i Certificates.p12 | gh secret set APPLE_CERTIFICATE
gh secret set APPLE_CERTIFICATE_PASSWORD
gh secret set APPLE_ID
gh secret set APPLE_PASSWORD
gh secret set APPLE_TEAM_ID
```

Then delete the `.p12` file. The next release is signed, notarized and stapled, so macOS opens it without asking. The updater archive is made from the same notarized app, so updates are notarized too. If a secret is wrong, the release fails rather than publishing an app that isn't notarized.

To sign a build on your own Mac with a Developer ID certificate in your login keychain, set the identity before building. Add `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` to notarize it too.

```bash
APPLE_SIGNING_IDENTITY="Developer ID Application" npm run build
```
