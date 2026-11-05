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

- **Waiting for you:** an `AskUserQuestion` or `ExitPlanMode` call with no result yet, or an Edit or Write call still open after a minute. File edits finish within seconds, so that one is at the permission prompt. A turn that ends with a question in the Claude app's next-step box is waiting for you too.
- **Working:** any other open tool call, for up to two hours even if it writes nothing (tool calls have taken up to about 70 minutes; after two hours the session most likely stopped mid-call). Otherwise, activity within the idle time.
- **Working in the background:** a turn that ended while a background command, subagent or monitor it started is still going, for up to two hours after the turn ended. Its result starts the next turn by itself; in a replay of 400 transcripts, background work started about half of all turns. A task counts as finished once a task notification with a status arrives, or Claude stops it.
- **Done:** a reply that ended the turn with nothing left in the background, or a turn you interrupted.

In While Claude works mode the Mac stays awake only while some session is working or a process tree is busy, so it can sleep as soon as everything is done or waiting for you. A question or an approval turns the Claude pill to "needs you" and sends a notification, on the phone too when phone alerts are on.

### Codex and Cursor

Codex and Cursor sessions show in the same list, marked with the agent's name, and count as working the same way, so the Mac stays awake for them too. Notifications about a question or about being done name the agent. Usage limits, auto-continue and background work are Claude's only.

**Codex.** The Codex app and the `codex` command write one rollout file per session under `~/.codex/sessions/<year>/<month>/<day>/`, and keep session names in `~/.codex/session_index.jsonl`. Every turn starts with a `task_started` record and ends with `task_complete`, or `turn_aborted` when you stop it. A turn that has started and not ended counts as working, for up to two hours after its last record, and one that has ended is done. Codex writes nothing while an approval prompt is open, so that counts as working too. An open `request_user_input` call, the terminal's way of asking you something, is waiting for you. The Codex app's questions don't hold up the turn, so they don't count. `codex` processes and what they start count toward the busy check, like Claude's.

**Cursor.** Cursor writes each agent chat to `~/.cursor/projects/<project>/agent-transcripts/<chat>/<chat>.jsonl`, with its subagents' beside it. The records carry no timestamps and no tool results, so activity is the file's modified time. Cursor writes a reply once it's complete, before its tools run, and from Cursor 3 on a `turn_ended` record when the turn is over. A reply with tool calls as the newest record is working, or waiting for you if it calls AskQuestion or CreatePlan. A turn that has ended is done, or waiting for you if its last reply asked. Chats from earlier Cursor versions, without `turn_ended`, go by the idle time. Cursor sessions count only while the Cursor app is open, and its CPU doesn't count: the editor is busy indexing and drawing whether or not its agent works.

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

Overnight notifies you when Claude asks you something, is done, hits a usage limit or doesn't continue after one; when it stops keeping the Mac awake (the stop time, low battery or heat, or the helper stopping); when something else changes the sleep setting; and when disk space runs low. Under Notifications, at the bottom of the popover, each kind has a Mac switch and, once phone alerts are on, a Phone switch. All of them start on. Settings from before this list keep their old Notify me when Claude needs me choice for the "is done" alert. That alert only follows work that lasted 5 minutes or more, so a quick reply or a scheduled check that runs for a minute doesn't send one.

## Phone alerts

Turn on Send alerts to my phone and Overnight creates a random topic on ntfy.sh. Install the ntfy app on your phone, subscribe to the topic and press Send a test alert. On an iPhone, Copy topic name and Universal Clipboard save you typing it. Scanning a QR code doesn't work there, because the iPhone app doesn't open `ntfy://` links.

Alerts never name a session or project. Anyone who knows an ntfy.sh topic name can read it, so keep the random one or point Overnight at your own ntfy server. With Only when I'm away on, the default, alerts go to the phone only while the lid is closed or nobody has used the keyboard or mouse for 5 minutes. An alert that can't be sent is written to the log.

Claude's own phone notifications go through Remote Control, which some organizations turn off. These alerts don't depend on it.

## Updates and opening at login

Every 6 hours, and a minute after it starts, Overnight reads `latest.json` from the newest GitHub release. When that names a newer version, it sends one notification and shows an Update bar in the popover. Nothing installs until you click Update, unless Install updates automatically is on. Then Overnight installs it by itself the next time it looks, every 30 minutes, while it isn't keeping the Mac awake and the popover is closed, and says so in a notification as it restarts. A download that fails isn't tried again until the next check finds the version. The archive it downloads, `Overnight.app.tar.gz`, is signed in the release workflow, and the app checks it against the public key in `tauri.conf.json` before it replaces itself and restarts. Update is turned off while Overnight is keeping the Mac awake, because restarting would end the night. Check for updates, under Overnight in the popover, turns the background checks off. Check for Updates in the menu bar menu checks at once either way.

Open at login is a LaunchAgent, `~/Library/LaunchAgents/Overnight.plist`, that starts the app's own executable. Overnight turns it on the first time it runs from an Applications folder, and never again after that, so switching it off in the popover sticks. A copy run from Downloads leaves it off.

## Files

Overnight keeps everything in `~/Library/Application Support/dev.local.overnight/`:

| File | What it holds |
| --- | --- |
| `settings.json` | Your settings |
| `events.jsonl` | The log, trimmed to about 2 MB |
| `ctl/` | The helper's `hold`, `release` and `pid` files |
| `cleanup.command` | The Terminal script that Ask Claude to clean up runs |
| `update-announced` | The last new version a notification went out for |
| `.login-default` | Marks that Open at login was turned on the first time |

## Resource use

Overnight reads the battery, the lid and the sleep setting in-process through IOKit and runs no commands on its regular checks. It checks every 10 seconds while it's keeping the Mac awake or the popover is open, and every 30 seconds otherwise. The popover's web page closes 3 seconds after the popover hides, which frees most of WebKit's memory. With the popover closed, the app uses around 20 to 30 MB. The root helper starts only `sleep` in its 5-second loop and checks the battery and the sleep setting every 30 seconds.

## Things to know

A Mac kept awake in a bag gets hot. The heat guard helps, but leave the Mac somewhere with airflow.

On a managed work Mac, check that your IT policy allows running `pmset` as an administrator.

Notifications ask for permission the first time the built app sends one. Under `npm run dev` they may appear under Terminal, or not at all.
