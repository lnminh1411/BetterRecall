# BetterRecall user guide

For BetterRecall 0.4.0-dev on Windows 11. Checkpoint 4b of the 0.4 plan extends this guide with the first-start tutorial.

## Install

1. Download `BetterRecall-Setup-<version>.exe` from [GitHub Releases](https://github.com/lnminh1411/BetterRecall/releases) and run it. It installs for your user only; no admin rights are needed.
2. Choose how to get the models: all of them now, the minimum set, or later from Settings > Models. Downloads resume where they stopped.
3. BetterRecall starts after install and every time you sign in; turn that off in Task Manager > Startup apps. Its icon sits in the notification area.

On first start BetterRecall asks for consent before it captures anything, then runs a short benchmark to pick settings for your PC.

## Everyday use

| Key | What it does |
|---|---|
| `Ctrl+Alt+Space` | Open search |
| `Ctrl+Alt+P` | Pause or resume capture |
| `Ctrl+Alt+R` | Save the instant replay (the last minutes of your screen) as a video |
| `Ctrl+Alt+Shift+X` | Panic: stop capture, clear memory, lock the history and, if set, delete the last few minutes |

Change them on the Shortcuts page. A key that another app already holds shows a warning there.

- **Search:** type what you remember, in your own words. Results come in three groups: text on screen, what the screen looked like, and speech. Filter by time, kind, app or website. Right-click a result to copy it, save it or delete it.
- **Timeline:** browse a day hour by hour.
- **Replays:** saved replay clips. Play them, export them or delete them.
- **Home:** what is running, with switches for each feature.

## Privacy controls

- **Pause:** from the tray, Home or `Ctrl+Alt+P`. Pauses can end on their own after a set time.
- **Exclusions:** Settings > Privacy lists apps, folders and window titles that are never captured. Private browser windows are always skipped.
- **Lock:** lock the history from Home or with a lock hotkey you assign on the Shortcuts page, or let it lock with Windows (Settings > Security). Panic locks it too. Reading it again needs Windows Hello or your unlock password.
- **Delete:** the Danger zone page deletes a time range, one app or everything. A result's menu in search deletes that capture, or everything from its app or website.
- **Cloud AI:** off by default. BetterRecall makes no network connections of its own except the ones you start: update checks (automatic checks are off by default) and model downloads.

## MCP (AI tools on your PC)

Turn on Settings > IPC and MCP > MCP server, then add BetterRecall to your AI tool:

The CLI lives at `%LOCALAPPDATA%\BetterRecall\app\bin\betterrecall.cmd` and is not on `PATH`:

```powershell
& "$env:LOCALAPPDATA\BetterRecall\app\bin\betterrecall.cmd" mcp clients
& "$env:LOCALAPPDATA\BetterRecall\app\bin\betterrecall.cmd" mcp add <client>
```

`betterrecall mcp snippet <client>` prints the config to paste by hand. The tools are read-only and return text only.

## Updates and uninstall

Settings > Update checks GitHub Releases and installs a signed update when you click Install. Uninstall from Windows Settings > Apps > BetterRecall; the uninstaller asks whether to keep your history.

## Problems

Report bugs on [GitHub Issues](https://github.com/lnminh1411/BetterRecall/issues). Security problems go to [private security advisories](https://github.com/lnminh1411/BetterRecall/security/advisories/new).
