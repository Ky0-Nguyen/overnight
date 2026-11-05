# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

The interface is HTML/CSS/JS rendered in a macOS menu bar popover (Tauri 2, WKWebView). It follows macOS conventions: system font, system status colors, light and dark appearance.

## Users

People who run Claude Code on a MacBook, in Terminal or in the Claude desktop app's Code tab, and leave it working on long tasks overnight or while away. They want to close the lid and walk off knowing that the Mac stays awake exactly as long as Claude has work, that it won't harm the battery or overheat, and that in the morning they can see what happened and what needs them.

Overnight is meant for public release, so copy and flows must not assume the reader knows how the app was built, what `pmset` is, or how Claude Code records its sessions.

## Product Purpose

Overnight keeps a MacBook awake with the lid closed while Claude works, then lets it sleep again when the work is done, the battery runs low, the Mac runs hot, or the stop time comes.

A good night: Claude's work finishes, including after a usage-limit reset; the Mac sleeps once nothing is left to do; the battery and temperature stay within the user's limits; and the morning log says what happened. If Claude stopped and needs the user, Overnight says so and says what to do.

## Positioning

Overnight knows what Claude Code is doing. It holds the lid lock while sessions are working, and releases it when Claude is done, waiting for the user, paused on a usage limit, or idle. Activity comes from session transcripts and the Claude process tree, not from a timer. It does this with one admin password per night and nothing installed system-wide.

## Operating Context

- Used twice a day: in the evening before closing the lid (arm it, check the guards), and in the morning (read the log, deal with stopped sessions).
- Lives in the menu bar with no Dock icon. The popover is 380×620 and closes when it loses focus.
- Reads Claude Code session transcripts in `~/.claude/projects`. The Claude desktop app also appends untimed bookkeeping records to them, so activity is taken from timestamped records, never from the file's modified time.
- Claude Code usage limits stop sessions until a reset time. Claude continues by itself after the reset only if auto-continue is on: `autoContinueAtUsageLimit` for Claude Code, or the Claude desktop app's per-card **Auto-continue when limits reset** checkbox.
- Uses macOS notifications for anything the user should know while away from the popover.

## Capabilities and Constraints

Capabilities today:
- Keep awake with the lid closed, in two modes: **While Claude works** and **All night**. Each has an optional **Stop at** time.
- Guards: battery floor (sleep below 10% by default), heat, and stop time. The root helper also has its own failsafe at 8% battery.
- Claude session list with status: *working*, *needs you* (a question or a file change to approve), *idle*, *paused* (usage limit, waiting for the reset), *stopped* (didn't continue after a reset), *not running*. A session whose turn ended counts as done straight away.
- Codex and Cursor sessions are listed with Claude's, marked with the agent's name, and keep the Mac awake while they work. Questions and "done" notes name the agent. Usage limits and background work are Claude's only.
- Usage limits: a paused session counts as not working, so the Mac doesn't stay up for a reset; Claude's auto-continue is expected to pick the work up. Overnight notifies if Claude didn't continue, and shows how to resume and how to turn on auto-continue.
- Notifications: each kind (Claude asks you something, is done, hits a usage limit, doesn't continue after one; stop time, low battery or heat, helper stopped, sleep changed outside Overnight; low disk space) has its own Mac and Phone switch, all on by default.
- Morning summary: the first open after a run turns the popover's top into "Last night": the run's times, how it ended, Claude's working time, time held awake and the battery change, plus any sessions that need the user.
- Outside changes: if something else turns sleep back on mid-hold, the helper puts the hold back within 30 seconds and the user is told; if something disables sleep outside a run, the user is told the guards aren't watching.
- Storage: free-space warning, a scan of common space hogs, and **Ask Claude to clean up**, which opens Claude Code in Terminal.
- Log: a timeline of each night with a summary of time held awake, time Claude was busy, and the battery change.
- Phone alerts through ntfy: the notifications the user picks for the phone, sent only while the user is away by default (lid closed, or no input for 5 minutes). They never name a session or project. Claude's own phone push needs Remote Control, which organizations can turn off (the author's has), so Overnight doesn't rely on it.
- Updates: checks GitHub every 6 hours, says when a version is out and installs it from the Update bar. **Install updates automatically**, off by default, installs it without asking once Overnight isn't keeping the Mac awake. Neither restarts the app mid-night.

Binding constraints (confirmed):
- **No permanent root.** Never install sudoers rules, launch daemons or anything system-wide. A root helper is started through the macOS admin prompt at most once per night and exits when released or when the app quits.
- **Never deletes files.** Cleanup is always handed to Claude in Terminal, where the user can watch and answer.
- **Stays light.** Memory and CPU must stay low enough that running all night costs next to nothing. Readings are taken in-process, the popover's page is closed while hidden, and the monitor slows down when not armed.
- **macOS only.**
- **The Mac's safety comes first.** When goals conflict, the battery floor and heat guard win, even if Claude is mid-task.

Terminology (use consistently):
- *working* / *idle* for Claude's activity. *Idle* means no activity for the idle time (15 minutes by default).
- *needs you* for a session waiting on the user: a question or a file change to approve, or a session stopped after a usage limit. Session rows say which (*asking you*, *needs approval*, *stopped*) and where to answer.
- *paused* / *stopped* for usage limits.
- *continue*, matching Claude's own **Auto-continue**.
- **Keep awake tonight** / **Let it sleep** for the main action.

Open decisions:
- Distribution: releases are built and published automatically on every push to main that changes the app (GitHub Releases, universal). The workflow signs them with a Developer ID and notarizes them once the Apple signing secrets are set; until then they're ad-hoc signed and users approve the first open in Privacy & Security. Still open: setting those secrets, and the minimum macOS version (currently 12).
- A license for the public repository (none chosen yet).
- First-run onboarding for people who aren't the author.
- Phone alerts: whether to support private ntfy servers that need an access token (it would belong in the Keychain), and whether to offer other services such as Pushover.
- An accessibility standard. The popover now meets WCAG AA contrast in both themes, has a visible focus ring, announces status changes and links its tabs to their panels; nothing formal has been decided beyond that.
- Wording now that Codex and Cursor count: the mode, the pill, the status lines and the log still say Claude.

## Brand Commitments

- Name: **Overnight**.
- Existing assets: the crescent-moon-and-star menu bar icons (off, standby, on) and app icon, drawn by `scripts/make-icons.py`.

## Evidence on Hand

- On the author's Mac, across 101 usage-limit stops, Claude continued by itself 4 times; the user restarted 68 by hand, and 29 never continued. This is the problem the usage-limit features address. Analysed from local transcripts, October 2026.
- Resource use measured on the author's Mac, 30 October 2026: about 70 MB across the app and its WebKit processes with the popover open, and about 25 MB for the app itself. When the popover closes, the WebKit page and graphics processes exit. No external commands run per tick.
- Subscribing by QR code doesn't work on iPhone. On 30 October 2026, the author scanned QR codes of `ntfy://ntfy.sh/<topic>` (with and without `?display=`) with the iPhone Camera. The Camera read both and said "No usable data found", meaning no iPhone app handles `ntfy://` links. ntfy documents these links for its Android app only. Subscribing on iPhone stays **Copy** plus Universal Clipboard.
- No users, testimonials, reviews or press exist. Do not invent any.

## Product Principles

1. **Safety before throughput.** The Mac's battery and temperature limits are never traded for more Claude time.
2. **Least privilege, for the shortest time.** Ask for root once a night, hold it only as long as needed, and leave nothing behind.
3. **Hands off the user's files.** Overnight observes and reports; Claude, supervised by the user, acts.
4. **Light enough to forget.** An all-night companion must cost close to nothing to run.
5. **Say what to do, not just what happened.** Every warning names the next step in plain words, in the same terms Claude uses.
