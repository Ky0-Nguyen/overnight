# How Overnight works

The technical side of Overnight: how it keeps the lid closed, how it reads what Claude Code is doing, and what it keeps on disk. For using it, see [the guide](https://dienp.github.io/overnight/). To build it, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Keeping the lid closed

Only root can stop a MacBook from sleeping when its lid closes (`pmset -a disablesleep 1`). When you press Keep awake tonight, macOS asks for your password once and Overnight starts a small root shell script, [helper.sh](src-tauri/src/helper.sh). For the rest of the night the app steers it through files in `~/Library/Application Support/dev.local.overnight/ctl/`. The script keeps the Mac awake while `hold` exists, and restores the original setting and exits when `release` appears. It writes its process ID to `pid` in the same folder.

The helper restores the original setting and exits as soon as the app quits or crashes. It lets the Mac sleep by itself at 8% battery, even if the app is stuck, and puts the hold back if something else turns sleep on again during the night.

Nothing is installed system-wide. There's no sudoers entry, launch daemon or kernel extension, and once the helper exits the Mac is back to normal.

## Telling when Claude is working

Claude counts as working when a Claude Code process is running and one of two things is true: a session under `~/.claude/projects` is working (see below), or one session's process tree, including its builds, tests and tools, has used at least 20% of a CPU core for 15 seconds. Each session counts on its own, because a dozen idle sessions together can pass 20% while none of them is doing anything. Sessions in Terminal and in the Claude desktop app both count.

A recorded event is a timestamped record, such as a prompt, a reply, a tool result or a retry, or a write by one of the session's subagents. Overnight ignores the transcript file's modified time, because the Claude desktop app also appends untimed bookkeeping (titles, cost, artifact state) to sessions nobody is working in.

### Done, waiting for you, or working

Claude Code writes nothing while a tool runs or a question or permission prompt is open, so Overnight reads where each session's turn stands from its newest conversation records:

- **Waiting for you:** an `AskUserQuestion` or `ExitPlanMode` call with no result yet, or an Edit or Write call still open after a minute. File edits finish within seconds, so that one is at the permission prompt.
- **Working:** any other open tool call, for up to two hours even if it writes nothing (tool calls have taken up to about 70 minutes; after two hours the session most likely stopped mid-call). Otherwise, activity within the idle time.
- **Done:** a reply that ended the turn, or a turn you interrupted.

In While Claude works mode the Mac stays awake only while some session is working or a process tree is busy, so it can sleep as soon as everything is done or waiting for you. A question or an approval turns the Claude pill to "needs you" and sends a notification, on the phone too when phone alerts are on.

## Usage limits

Claude Code records a usage-limit stop in the session transcript in one of two ways. When the session gives up, it writes an assistant record with `"isApiErrorMessage": true, "error": "rate_limit"` and the reset time, in epoch seconds, in `quotaLimits.resetsAt`. When it waits for the reset and retries (this needs `CLAUDE_CODE_RETRY_WATCHDOG=1`), it writes a system `api_error` record with the reset time in `error.rateLimits.resetsAt`. A session counts as waiting until a normal reply follows.

Whether Claude continues by itself after the reset depends on these settings, which belong to Claude rather than Overnight:

| Where | Setting |
| --- | --- |
| Claude Code, `~/.claude/settings.json` | `"autoContinueAtUsageLimit"`, on unless set to `false` (Claude Code 2.1.234 or later) |
| The `env` block in `~/.claude/settings.json` | `"CLAUDE_CODE_RETRY_WATCHDOG": "1"` keeps retrying and waits until the reset |
| Claude desktop app, Code tab | The Auto-continue when limits reset checkbox on a session's usage-limit card |

The Claude Code docs cover [waiting for a usage limit to reset](https://code.claude.com/docs/en/interactive-mode.md#wait-for-a-usage-limit-to-reset) and the [autoContinueAtUsageLimit setting](https://code.claude.com/docs/en/settings-reference.md#autocontinueatusagelimit).

Overnight doesn't keep the Mac awake through a reset. In While Claude works mode a session paused on a limit counts as not working, so the Mac can sleep while it waits, and Claude's auto-continue picks the work up after the reset. If a session still hasn't continued 10 minutes after the reset, Overnight tells you and shows how to resume it.

## Notifications

Overnight notifies you when Claude asks you something, is done, hits a usage limit or doesn't continue after one; when it stops keeping the Mac awake (the stop time, low battery or heat, or the helper stopping); when something else changes the sleep setting; and when disk space runs low. Under Notifications, at the bottom of the popover, each kind has a Mac switch and, once phone alerts are on, a Phone switch. All of them start on. Settings from before this list keep their old Notify me when Claude needs me choice for the "is done" alert.

## Phone alerts

Turn on Send alerts to my phone and Overnight creates a random topic on ntfy.sh. Install the ntfy app on your phone, subscribe to the topic and press Send a test alert. On an iPhone, Copy topic name and Universal Clipboard save you typing it. Scanning a QR code doesn't work there, because the iPhone app doesn't open `ntfy://` links.

Alerts never name a session or project. Anyone who knows an ntfy.sh topic name can read it, so keep the random one or point Overnight at your own ntfy server. With Only when I'm away on, the default, alerts go to the phone only while the lid is closed or nobody has used the keyboard or mouse for 5 minutes. An alert that can't be sent is written to the log.

Claude's own phone notifications go through Remote Control, which some organizations turn off. These alerts don't depend on it.

## Files

Overnight keeps everything in `~/Library/Application Support/dev.local.overnight/`:

| File | What it holds |
| --- | --- |
| `settings.json` | Your settings |
| `events.jsonl` | The log, trimmed to about 2 MB |
| `ctl/` | The helper's `hold`, `release` and `pid` files |
| `cleanup.command` | The Terminal script that Ask Claude to clean up runs |

## Resource use

Overnight reads the battery, the lid and the sleep setting in-process through IOKit and runs no commands on its regular checks. It checks every 10 seconds while it's keeping the Mac awake or the popover is open, and every 30 seconds otherwise. The popover's web page closes 3 seconds after the popover hides, which frees most of WebKit's memory. With the popover closed, the app uses around 20 to 30 MB. The root helper starts only `sleep` in its 5-second loop and checks the battery and the sleep setting every 30 seconds.

## Things to know

A Mac kept awake in a bag gets hot. The heat guard helps, but leave the Mac somewhere with airflow.

On a managed work Mac, check that your IT policy allows running `pmset` as an administrator.

Notifications ask for permission the first time the built app sends one. Under `npm run dev` they may appear under Terminal, or not at all.
