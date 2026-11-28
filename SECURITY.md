# Security policy

BetterRecall keeps an encrypted history of what you see and hear, so security reports matter a lot to us.

## Reporting a vulnerability

Report it privately through a GitHub security advisory: https://github.com/lnminh1411/BetterRecall/security/advisories/new

Please do not open a public issue for a vulnerability. Include the version, Windows build, what an attacker can do, and the steps to reproduce. Do not attach real captured data.

We aim to answer within 7 days and to ship a fix for confirmed high-severity problems within 30 days. We credit reporters in the release notes unless you ask us not to.

## Supported versions

Only the latest release gets security fixes. BetterRecall updates itself to the latest release when you install an update from Settings > Update.

## Scope

In scope: the agent, worker, UI, CLI, MCP server, installer and updater in this repository, the encryption of stored data, the pipe trust checks, and the update signature checks.

Out of scope: attacks that need administrator rights or full control of the user's account (they can read the screen directly), and problems in Windows, drivers or third-party AI providers you configure yourself.
