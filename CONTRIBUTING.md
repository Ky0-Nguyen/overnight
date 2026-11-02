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

To work on the popover without Tauri, serve `src/` (for example with `python3 -m http.server -d src`) and open `index.html`. It loads `preview-mock.js` in place of the backend. Query parameters choose the state: `state=off`, `on` or `standby`; `limit=waiting` or `missed`; `auto=off`; `ask=question` or `approval`; `phone=on`; and `theme=dark`.

`npm run icons` redraws the menu bar and app icons from `scripts/make-icons.py`.

## Releases

Every push to `main` that changes the app, whether a merged pull request or a direct push, makes a release through [the release workflow](.github/workflows/release.yml). It works out the next version with `scripts/release-version.mjs`, runs the tests, builds a universal app for Apple silicon and Intel, ad-hoc signs it, and publishes `Overnight-macos.zip` on the [Releases page](https://github.com/dienp/overnight/releases). Then it commits the new version to `main` and tags it.

The version goes up by a patch number each time. Put `[minor]` or `[major]` in a commit message for a bigger step, or `[skip release]` to leave a push out. Changes that only touch `docs/` or Markdown files don't release. To see what the next version would be:

```bash
node scripts/release-version.mjs --dry-run
```
