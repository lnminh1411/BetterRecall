# BetterRecall user guide

For BetterRecall 0.4.0-dev on Windows 11. Checkpoint 4b of the 0.4 plan extends this guide with the first-start tutorial.

## Install

1. Download `BetterRecall-Setup.exe` from [GitHub Releases](https://github.com/lnminh1411/BetterRecall/releases) and run it. It installs for your user only; no admin rights are needed.
2. Choose how to get the models: all of them now, the minimum set, or later from Settings > Models. Downloads resume where they stopped.
3. BetterRecall starts after install and, unless you turn it off, every time you sign in. Its icon sits in the notification area.

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
- **Exclusions:** Settings > Privacy lists apps and websites that are never captured. Private browser windows are always skipped.
- **Lock:** lock the history from the tray or Home, or let it lock with Windows (Settings > Security). Reading it again needs Windows Hello or your unlock password.
- **Delete:** the Danger zone page deletes a time range, one app or everything. A result's menu in search deletes that capture, or everything from its app or website.
- **Cloud AI:** off by default. While it is off, BetterRecall makes no network connections except the update check and model downloads.

## MCP (AI tools on your PC)

Turn on Settings > IPC and MCP > MCP server, then add BetterRecall to your AI tool:

```powershell
betterrecall mcp clients
betterrecall mcp add <client>
```

`betterrecall mcp snippet <client>` prints the config to paste by hand. The tools are read-only and return text only.

## Updates and uninstall

Settings > Update checks GitHub Releases and installs a signed update when you click Install. Uninstall from Windows Settings > Apps > BetterRecall; the uninstaller asks whether to keep your history.

## Problems

Report bugs on [GitHub Issues](https://github.com/lnminh1411/BetterRecall/issues). Security problems go to [private security advisories](https://github.com/lnminh1411/BetterRecall/security/advisories/new).
