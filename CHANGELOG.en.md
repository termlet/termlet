# Changelog

[简体中文](CHANGELOG.md) · **English**

This file lists the changes you will notice in each version, newest first. Versions follow [Semantic Versioning](https://semver.org/): before 1.0, a minor version bump (0.x) may bring incompatible changes, which are noted in that version. Each version's heading gives the version number and release date (changes not yet released go into a section marked "unreleased" at the top, which gets its date on release), and its changes are grouped into Added, Changed, Fixed and Security (empty groups are left out; the first version is summarized by area instead). Any change to the anonymous usage statistics collected is noted in the version that makes it. For downloading and installing, see [Installation](README.en.md#installation) in the README.

## 0.3.3 (unreleased)

### Added

- The MCP server can now be reached over HTTP: under Settings → AI & automation, set the MCP server's transport to "HTTP" and turn on listening, and Termlet serves MCP on this computer at `127.0.0.1:47800` (the port can be changed); AI tools connect with the address and a token. It's off by default and only accepts connections from this computer (web pages in a browser can't connect); every request needs the token (Termlet checks it against a digest only, and you can regenerate it at any time); and since Termlet itself listens, it only works while Termlet is running. Claude Code, Codex, Cursor, VS Code (GitHub Copilot), Gemini CLI and generic JSON for other tools have configurations that include the token when copied and all go in user-level configuration; Claude Desktop can only use standard input and output. Tools, approval of dangerous commands and the audit log work as with standard input and output; the audit log's Source shows "MCP·HTTP (client name)" and records the process that opened the connection (Windows, Linux), and AI tools on a server can use it through SSH remote forwarding. It can also listen on "This computer and LAN": only connections from private ranges are accepted, with a separate LAN token that can be regenerated on its own; the other computer can only list hosts and run commands (it can't upload or download files on this computer), and the audit log and approval dialog show its address. Traffic on the LAN is unencrypted, so use it only on trusted networks; the settings page and user guide explain the risks, the Windows Firewall prompt, and encrypted alternatives such as SSH forwarding and Tailscale.
- Keyword highlighting: IPv6 addresses are now marked by default (full and `::`-compressed forms, zones such as `fe80::1%eth0`, `/64` prefixes, `::ffff:192.0.2.1`, and bracketed forms with a port such as `[::1]:22`; times, MAC addresses, certificate fingerprints and code such as `std::vector` are not mistaken for IPv6), added on upgrade if you never changed the rules. New "Add Preset": besides the defaults, you can add URLs, MAC addresses, UUIDs, dates and times, and numbers (with their units, such as `230ms` and `1.5GiB`); presets already in the list are checked, they're inserted in a fixed order (URLs first, numbers last) so URLs and dates aren't colored in pieces, a default rule deleted by mistake can be added back, and presets in the list show their name in front of the pattern. Colors can be customized: red, green, yellow, blue, magenta and cyan still take whichever is clearer in the color scheme (they look the same after the update), the new Dark Red and so on always use colors 1–6 of the terminal color scheme and Bright Red and so on colors 9–14, and "Custom Color…" picks any color with a color picker or a typed value, darkened or lightened automatically when it isn't clear enough on the background (contrast below 3:1); the swatches, menu and preview in Settings show the color as it actually appears on the current scheme. When synced to 0.3.2 or earlier, rules with a dark, bright or custom color are shown in red (the rules themselves are kept), and saving the settings in those versions changes their color to red.
- 13 new built-in terminal color schemes, 39 in total: One Dark Pro, Ayu Dark / Light, VS Code Dark 2026 / Light 2026, Monokai (VS Code) (the Monokai theme that ships with VS Code), Tomorrow Night, Catppuccin Frappé / Macchiato, Rosé Pine Moon, Nightfox / Dayfox and Kanagawa Lotus, each taken from the project's own files, with its source and license.
- SFTP online editing supports UTF-16 (LE and BE). Files with a BOM (text redirected with `>` in Windows PowerShell 5.1, .reg files, some Windows logs and exported CSV files) open directly, and files without a BOM are detected too when it's clear. Saving keeps the original encoding, BOM and line endings, and characters such as emoji are saved correctly. The status bar shows the encoding (such as "UTF-16LE BOM"), and "Reopen with another encoding" offers UTF-16LE and UTF-16BE. The encodings for host settings, terminals and SFTP file names are unchanged (UTF-8, GBK / GB18030, Big5).
- Three new sources under Import Hosts. “NyaTerm (local data or .nya backup)” reads the local database (`~/.nyaterm/nyaterm.redb`; quit NyaTerm first) or an encrypted `.nya` backup exported in NyaTerm’s settings (enter the password set when exporting): SSH, Telnet and serial connections come with groups, notes, tags, encodings, X11, commands run after login, jump hosts, proxies (SOCKS5, HTTP, proxy commands), port forwards and quick commands, and NyaTerm “accounts” become credential profiles (an existing profile with the same name and username is reused); attached OpenSSH certificates aren’t imported and agent forwarding is turned off. “ShellRS (data folder or sync folder)” reads the local `shellrs.db` by default (even while ShellRS is running): SSH, Telnet and serial hosts come with groups, notes, jump hosts, HTTP / SOCKS5 proxies, port forwards and commands run after login, command snippets become quick commands, credentials become credential profiles (password, private key or SSH agent), and hosts set to “No password” use ssh-agent. ShellRS keeps passwords in the system keychain, so they aren’t imported from the data folder; if ShellRS syncs, choose its sync folder instead and enter the sync password to import login passwords, proxy passwords, credential passwords, private keys and one-time code secrets too. “NyaTerm quick commands (exported .json)” (issue #2) turns categories into groups (nested ones written as “Parent / Child”), converts variables and escapes from NyaTerm’s rules to Termlet’s, adds a trailing line break to “execute” and line-by-line commands, and doesn’t import snippets identical to existing ones; “Import Snippets…” on the Snippets page and in the command palette opens it with that source chosen. Imported passwords and one-time code secrets go into the vault and private keys into the key library, and port forwards aren’t started automatically; anything that doesn’t map (pauses `\p`, option lists, sending intervals, unrecognized variables, descriptions, colors, icons, pinning and risk levels in quick commands; multi-hop jump chains or custom login prompts in ShellRS) is spelled out in the preview. Importing terminal color schemes also recognizes the `nyaterm-theme.json` exported from NyaTerm’s theme designer (its terminal’s 16 colors plus foreground, background, cursor and selection colors).

### Changed

- MCP server and command-line tool: you now pick the AI tool for the MCP configuration from a drop-down, which adds VS Code (GitHub Copilot, in `~/.copilot/mcp-config.json`), Gemini CLI and generic JSON for other AI tools; Claude Code, Codex and Gemini CLI get both their `mcp add` command and the configuration file, each with its own Copy button. For Claude Desktop, you now find the configuration file through its Settings → Developer → Edit Config (on Windows its location depends on how it was installed, so the path given before wasn't always right). The Codex configuration raises `tool_timeout_sec` from 300 to 900 (the longest a single command may run, including the time you take to approve it, before Codex stops it) and explains how to skip the confirmation Codex itself asks for before every command, upload and download. When a dangerous command comes from MCP, the approval dialog's title names the AI tool (such as "Claude Code (MCP) wants to run a dangerous command on …"; common tools by product name, others by the name they report), and tools without a brand logo get a generic AI icon; before, it always said "The command-line tool". When an AI tool asks for it (a `progressToken` in the request, which Claude Code and Codex both send), the MCP server reports progress every 15 seconds while a command runs, so longer commands are no longer cut off by tools that stop calls showing no progress. Canceled calls (the AI tool stopped partway, or the command-line tool or AI tool quit) show as "Canceled" in the audit log instead of "Failed". When an OpenSSH login certificate (including one used for a jump host) expires in less than an hour, `termlet exec`, `upload` and `download` run as usual and add a line on stderr saying when it expires, about how many minutes are left and which jump host it's for; the JSON gets an `expiringCertificates` field (plus `jump` for a jump host's, left out when no certificate is about to expire), and MCP results warn you the same way. Before, only the terminal warned you, so command-line and AI calls found out only once the certificate had expired and they couldn't connect.
- When a host's name resolves to both IPv6 and IPv4 addresses, both are now tried in parallel (Happy Eyeballs): the address your system prefers goes first, and if it hasn't connected within 250 milliseconds the other kind is tried at the same time; whichever connects first is used. Before, addresses were tried one at a time with up to 15 seconds each, so with broken IPv6 on your computer you waited for it to time out before IPv4 was tried; the connection timeout is now 15 seconds in total. This applies to SSH, Telnet, connecting to proxies, port forwarding and "Lend Local Proxy to the Server" connecting to targets on this computer, and X11 forwarding connecting to your X server, so writing `localhost` for something that listens on IPv4 only no longer costs about 2 seconds on Windows either.
- The local address of local and dynamic forwarding can now be `::` (all IPv6 addresses), with the same warning as 0.0.0.0 that other devices on the network can reach it; 0.0.0.0 listens on IPv4 only and `::` on IPv6 only, so you can add one rule for each on the same port. Before, only loopback addresses and 0.0.0.0 were accepted.
- The "Command-line tool" page in Settings is now called "AI & automation", since it covers more than the termlet command: the Agent Skill, the MCP server, the audit log and the dangerous command rules too. The switch at the top now reads "Allow access from AI tools and scripts". The page of the same name in the host dialog is renamed too, and its options now read "Allow AI tools and scripts to use this host" and "Use a separate credential for AI tools and scripts". Error messages from the termlet command and the Agent Skill use the new name (an installed Agent Skill shows that an update is available).
- When a search in the color scheme library finds nothing, it now suggests importing scheme files from other terminals or reading the schemes in Windows Terminal, Xshell, MobaXterm and Tabby installed on this computer, with a button for each. The import dialog starts reading as soon as you switch to "Installed clients".
- In highlighting regexes, `\B` now uses ASCII word boundaries like `\b` (text right next to Chinese counts as a boundary), which makes matching faster on lines with Chinese text.
- When you double-click a file whose encoding can't be detected (not UTF-8 or UTF-16 text), the prompt now lets you open it in an encoding you choose (GBK / GB18030, Big5, UTF-16LE, UTF-16BE) as well as download it. Before, it could only be downloaded, so GBK and Big5 files couldn't be opened to switch the encoding.
- In online editing, a BOM at the start of a file no longer stays in the editor as an invisible character; it is still written back when you save (UTF-8 files with a BOM too). Before, text typed after Ctrl+Home went in front of the BOM, so the saved file no longer started with it.
- Importing from FinalShell: groups now use the folders’ names (from `folder.json`, nested as in FinalShell) instead of their IDs (the folder names under `conn`), and connections in FinalShell’s recycle bin and RDP and other non-SSH connections are no longer imported as SSH hosts (non-SSH ones count as skipped in the preview). Descriptions, terminal encodings and saved login passwords now come along too: FinalShell’s saved passwords can be recovered without any password, so, as with Electerm and Netcatty, they’re imported only when you tick “Import saved passwords” below the preview (off by default); they go into the vault, and importing again only fills in the ones not saved yet. You can choose FinalShell’s data folder (its install folder), the `conn` folder in it, or a folder exported with right-click in FinalShell, and the default locations now include the install folder recorded in the registry, `%APPDATA%\finalshell`, and the locations on macOS and Linux. Connections with the port written as a string, or without a username, are imported too (the latter with a reminder to fill it in). Private keys, proxies and port forwards that FinalShell keeps itself aren’t imported; the preview notes them for each host.
- A leaner interface: the three encodings in the terminal tab menu are now under a "Server encoding ›" submenu (as in the terminal's context menu); "Lend Local Proxy…" is now "Lend Local Proxy to the Server…"; "Open SFTP Beside the Terminal" and "Open SFTP on the Right (Follow the Terminal's Folder)" are now both "Open SFTP on the Right"; the captions at the top of the sidebar's Keys, Profiles and Tunnels pages are gone, and the tip at the bottom of the Snippets page only shows while there are no snippets; the command palette no longer repeats the same icon and "Command" on every row when it lists only actions; several descriptions in Settings are shorter (icons, taskbar progress, tab position, interface size, resource usage, notifications when commands finish, command suggestions, allowing access from AI tools and scripts), the app lock and master password descriptions explain how the two differ, and the "Check before pasting" description says where the dangerous command rules are set; and key names on the Shortcuts page are written the same way (Esc, Page Up and so on). The start page and the empty host list now offer "Import hosts…". In the host dialog and credential profiles, fields that only had placeholder text (proxy, TOTP secret, key source, key file, certificate, key passphrase and so on) now have labels, and the key file has a "Browse…" button.

### Fixed

- The buttons in the find bar of terminals and replays, in the replay toolbar, the encoding and "proxy" buttons on the terminal's status bar, and the buttons in the file transfer bar: while a drop-down or context menu next to them was open, moving the pointer over them showed their tooltips on top of the menu.
- The "recording" and "root" badges on tabs: if the pointer rested on a badge while its tab's context menu was open, the tooltip didn't show after the menu closed until you moved the pointer off the badge and back.
- With "Right-click in the terminal" set to show a menu, right-clicking the terminal area or a replay showed no menu (right-clicking the tab worked). When a program has turned on mouse reporting, right-clicks still go to the program.
- When you first connected to a host at an IPv6 address on a port other than 22, the title of the fingerprint dialog read "::1:2222", so you couldn't tell which part was the port; it now reads "[::1]:2222".
- Switching pages in the host dialog kept the previous page's scroll position, cutting off the top of the new page; it now goes back to the top.

## 0.3.2 (2026-11-19)

### Changed

- The Agent Skill now lists Pi: Pi reads the shared folder `~/.agents/skills`, so Settings → Command-line tool names it and detects it when Pi is installed; from the command line, `termlet skill install --agent pi` works too (same as `--agent agents`).
- When the command-line tool or MCP asks you to approve a dangerous command, the seconds left before it's blocked now count down in the dialog; before, the number was fixed when the dialog opened.
- For commands you approve, the duration now covers only running them, not the time it took you to click "Allow": the time spent waiting for approval is reported separately (`approvalWaitMs` in the JSON from `termlet exec`, "plus … waiting for the user's approval" in MCP results, "Waited … for approval" in the audit log details). Older audit entries are unchanged and their duration includes the wait.
- The audit log's Source now tells MCP apart: calls through the MCP server show as "MCP (client name)" and can be filtered on their own under "All sources"; before, they were mixed in with the command-line tool and you had to open the details to see the caller. Older entries are recognized too. In CSV and JSON exports these entries have the source `mcp`, and the CSV gets an `approval_wait_ms` column at the end.
- "Delete All…" in the crash records now asks first and says this can't be undone.

### Fixed

- After actions in Settings that run right away (importing host CAs from known_hosts, copying the version info, installing the Agent Skill, changing PATH and so on), the pop-up message was hidden behind the Settings dialog, leaving only its end or nothing visible; the result now appears at the bottom of the dialog. Export and copy results in the audit log and crash report dialogs now appear inside those dialogs too.
- In Chinese and Japanese text, when the reason on a failed-connection page wrapped, a closing "。" could end up alone at the start of a line; the explanations shown when host tools are unavailable or need root, and the reasons for failed SFTP transfers, had the same problem.
- Results of "Export Public Key…" and removing a certificate in the public key dialog, and those in the Command History, Lend Local Proxy to the Server, Clean Up Docker, host tools' root permission dialog (copying a command) and "Open remote folder in …" (when writing ~/.ssh/config fails) dialogs and of reading a certificate file in the host and credential profile dialogs, were also hidden behind the dialog; they now appear inside it.
- The icon of a message inside a dialog sat higher than its text.
- The result of exporting diagnostics on Settings → About appeared at the very bottom of the page while the previous "Diagnostics copied" stayed at the bottom of the dialog; it now appears at the bottom of the dialog like other results, replacing the previous one. After deleting crash records, "Crash record copied" no longer stays.
- In Settings, switching from a category scrolled down to another one left the new category scrolled down as well (easy to click the wrong thing); it now starts at the top.
- When you right-clicked a row in the sidebar, a tab, a snippet, the file list and so on after its tooltip had appeared, the tooltip floated over the context menu and covered its first item.
- With no hosts, the sidebar's hint wrapped so that the last character and the full stop sat alone on the second line (in Chinese); its two sentences now take a line each.
- The X11 forwarding hint on a host's Connection page and in the user guide was wrong: VcXsrv comes with xauth, which Termlet looks for in its install folder, so the default "untrusted" mode works with it; only X servers without xauth, such as X410, need "Trusted".

### Security

- Updated the pageant dependency to 0.2.5: with agent forwarding on for a host and Pageant as the local agent (Windows), a malicious or compromised server could send a message claiming a huge size over the forwarding channel and make Termlet use memory until it crashed (denial of service, GHSA-662x-8wvj-2276). Agent forwarding is off by default.
- Updated the russh dependency to 0.65.1, which fixes two low-severity issues (GHSA-9264-jvgf-6w2f, GHSA-736f-c93q-9fwx) in a key-exchange callback and the agent server that Termlet doesn't use; Termlet isn't affected by them.

## 0.3.1 (2026-11-17)

### Changed

- On Windows, "Restart and install" now shows an "Updating Termlet" progress window that closes and reopens Termlet when it's done; before, nothing was visible for those few seconds and it was easy to think nothing had happened. This already applies when updating from 0.3.0.
- Vertical tabs: "Focus tab list" now has a default shortcut, Ctrl+Shift+J (⇧⌘J on macOS); then choose a tab with the arrow keys. If you've already given that combination to another action in Settings, your setting wins.
- The host tools icon at the right end of the terminal's status bar is gone: click the status bar itself, right-click it and choose "Show overview", or press Ctrl+Shift+O (⇧⌘O on macOS) to open the overview as before.
- The host tools panel's sections are easier to tell apart: each Overview section, and the lists and details on the Processes, Services, Docker and Network pages, now sit on their own bordered cards; the process list still shows the User column at the default width.

## 0.3.0 (2026-11-15)

### Added

- **Open source**: Termlet is now open source under [AGPL-3.0-or-later](LICENSE), with the source code at [github.com/termlet/termlet](https://github.com/termlet/termlet); Settings → About has a new Source code link, and the packages include the full license text. Packages are now built and signed by GitHub Actions and come with GitHub build provenance attestations, so `gh attestation verify` can show which build of this repository produced them. See [CONTRIBUTING.en.md](CONTRIBUTING.en.md) to get involved.
- **Host tools**:
  - A new Overview page: system and kernel, uptime, 10-minute charts for CPU (with per-core, iowait and steal), memory and network (hover for readings), each disk with inode usage, and the processes using the most CPU. Clicking the status bar at the bottom of a terminal opens it.
  - The Overview can pop out into a small always-on-top window (one per host, with its position and size remembered per host; it can't stay on top on Linux), which can reconnect by itself after a disconnect.
  - Processes: shows the systemd service and container each process belongs to, and can hide processes inside containers; a tree view and a filter by user; wide panels show the command line and can merge processes with the same name.
  - Services: the details show why a service failed, its exit code, how long it has run, restarts, memory and recent logs; services are grouped into custom and system ones; a new Timers sub-page (showing the server's time zone, with your local time on hover when they differ).
  - Docker: containers show their CPU and memory use, and unhealthy ones get their own filter; selecting a container shows its details (ports can be forwarded to this computer, bind mounts opened in SFTP, health checks, recent logs); a new Clean Up… lists what would be removed and how much space it frees before running.
  - The log viewer is now a tab of its own: choose the number of lines and time range, filter by level, filter and highlight keywords, follow new lines, with times in your local time zone.
  - Jump between processes, services, containers and ports; listening ports merge IPv4 and IPv6 and show the container that publishes them; web ports can be opened in your browser (forwarded to this computer automatically, until the connection closes).
  - Without passwordless sudo, actions that need root (starting and stopping services and timers, enabling them at boot, ending other users' processes) can run in the terminal instead; the confirmation shows the exact command, and the page refreshes when it finishes.
  - The panel can be used entirely from the keyboard; Ctrl+Shift+O (⇧⌘O on macOS) opens the Overview.
- **Resource status bar** at the bottom of terminals: a full mode (fixed positions, yellow and red above thresholds, the fullest disk, load per core, a small network chart) and a compact mode (latency, CPU and memory, plus anything that needs attention). Switch in Settings or by right-clicking the bar.
- **MCP server**: `termlet mcp` lets AI tools such as Claude Code, Claude Desktop, Cursor and Codex list hosts, run commands and transfer files, with the same approvals, audit log and dangerous-command rules, and without handing credentials to the AI. Settings → Command-line tool gives the configuration for each tool.
- **Cloud sync** can use S3-compatible storage and GitHub Gist; **scheduled backups** make a full backup daily or weekly and keep only the latest few.
- **Connections**:
  - A proxy command (ProxyCommand) is confirmed on each computer before it first runs; groups can set a proxy command; failures show the exit code and error output.
  - X11 forwarding (per host; untrusted mode by default).
  - Share local proxy: lends this computer's HTTP proxy to a server that can't reach the internet, with ready-made settings and undo commands for apt, dnf, yum, Docker and more.
  - When a server has no SFTP, file management and the command-line tool's uploads and downloads use SCP.
  - OpenSSH certificates: signing in with a private key presents the `-cert.pub` certificate next to it, or one you choose for a host or credential profile; private keys in the keychain can carry a certificate, whose details are shown; you're warned in the terminal when a certificate (including a jump host's) is about to expire. Settings → Host Keys can trust host CAs (importable from `@cert-authority` lines in known_hosts), so host certificates they sign need no fingerprint confirmation.
- **Files**: open a remote folder in VS Code or Zed (shown when the editor is installed); transfer progress on the Windows taskbar button, the macOS Dock, and Linux taskbars that support the Unity launcher interface (such as Ubuntu Dock and KDE).
- **Terminal**: session recording (asciicast format; input isn't recorded by default) and a playback tab (seek, speed, search; uses the recording's colors by default); command suggestions from your command history (off by default; needs shell integration).
- **Tabs and windows**: move a tab to a new window or another window, or drag it out of the window; terminals stay connected, transfers keep going, and port forwards and remote files open in the editor move along with it.
- **Vertical tabs**: Settings → General can put the tabs in a column on the left, grouped by host if you like, or collapsed into a narrow strip of icons (with badges for disconnected, bell, new output and so on; hover to expand); split panes get a group each, and the list works from the keyboard.
- **Open at startup**: chosen hosts or a local terminal; **title bar toolbar**: show, hide and reorder buttons, with a More menu when they don't fit.
- **Import** hosts from Electerm and Netcatty (saved passwords only when you tick the option).
- **Interface size**: 90%–150%, following Windows' text size by default.
- **Japanese and Korean** for the interface and the Windows installer (whose license notice shows in English).
- Windows arm64 and Linux arm64 packages (experimental: tested in continuous integration, not on real hardware).

### Changed

- Host tools: confirmation dialogs name the host and the exact command; the row being acted on shows progress and then the result; with passwordless sudo the permanent "not root" notice is gone and the Network page shows every process name; colors mean the same thing on every page; the split between list and details can be dragged; common actions appear on the row under the pointer; the panel remembers the last page per host; when an action fails, the reason appears on its row and under the info line (expand or copy it) instead of covering the panel title.
- Terminal rendering is faster (0–17% per frame).
- Confirmation and alert dialogs scale with the interface size; the macOS View menu can zoom the interface in and out and reset its size.

### Fixed

- The host tools panel sometimes showed the host of another tab (after clicking in the panel, while the window was in the background, or after a dialog closed), which could lead to acting on the wrong host.
- Without D-Bus the Services page showed "0 services" with no explanation; following logs from the context menu didn't use sudo.
- Containers with bind mounts lost their Compose project grouping.
- Uploading with trz inside tmux sometimes went unrecognized and never started.

### Security

- The program log no longer records the output of host tools commands (which may contain sensitive data from the server), only a summary of the action and its result.

### Usage statistics

- The interface language (`lang`) gains two values, `ja` and `ko` (Japanese and Korean interfaces).

## 0.2.1 (2026-11-04)

### Changed

- The title bar shows the update status: while a new version downloads it shows the progress, and clicking it shows the size, speed and time left; once downloaded it offers to restart and install. Settings → About also shows the size and speed next to the progress bar. With automatic update checks turned off, the title bar doesn't prompt.

### Fixed

- On macOS, Linux and the Windows portable edition, a new version wasn't announced anywhere in the main window, only on the About page; the title bar now shows "New version available", and clicking it opens the download page at the instructions for your kind of installation.

## 0.2.0 (2026-11-04)

### Added

- Windows portable edition (`termlet-<version>-win-x64-portable.zip`): unzip and run; data stays in the data folder next to the program, and it doesn't change PATH or create shortcuts. The first time it opens, you choose between a master password (so it works on other computers too) and using it only on this computer.
- Linux also gets deb, rpm and AppImage packages: deb and rpm install their dependencies through the package manager and put the command-line tool at `/usr/bin/termlet`; the AppImage runs as a single file. Settings → Command-line tool explains the setup for each.
- The tab menu gains Close Others, Close to the Right and Close All (they act on that pane and ask once for everything that needs confirming) and Show Host in Sidebar (also in the command palette); middle-clicking a tab closes it.
- In master password mode, the master password can be changed under Settings → Security; this affects only this computer.
- A key's public key can be exported to a file (.pub).
- Settings for the cursor shape (block, bar or underline) and cursor blinking; a shape set by the running program takes precedence.
- Windows: closing the window can minimize Termlet to the notification area (turn it on under Settings → General; off by default); sessions keep running, and you can restore or quit from the tray icon.
- The terminal reports light/dark theme changes: when the interface or terminal colors switch between light and dark, programs that support it, such as Neovim and Claude Code, switch too.
- macOS: Option can act as the Meta key; Termlet stays in the Dock after its last window closes, and clicking the icon opens a window again; quitting from the Dock, shutting down or logging out asks first when connections or transfers are open; the Window menu lists open windows in the Chinese interfaces too. These haven't been tested on a real Mac yet.

### Changed

- When the tabs don't fit, the mouse wheel scrolls the tab bar sideways.
- The resource details in the status bar no longer run off the terminal: long lists of network interfaces and disks scroll, and the heading shows the number of interfaces.

## 0.1.3 (2026-11-03)

Includes the changes in 0.1.1 and 0.1.2.

### Added

- The main window remembers its position, size and whether it was maximized, and opens there next time; if that screen is no longer connected, it opens centered on the main screen.
- The "⋯" button in the SFTP list header (or right-clicking the header) lets you choose whether to show size, modified time and permissions; the choice is remembered.
- Settings → About now links to the website, changelog, feedback email, license agreement and third-party licenses; "Copy version info" copies the version, system and architecture, ready to paste into a bug report.

### Changed

- The SFTP file list shows the modified time by default: as a pane narrows, permissions go first, then size, and the time last. Times use a compact form, the same in both panes, and hovering shows the full time to the second.

### Fixed

- On Windows and Linux, Ctrl+C in the terminal didn't interrupt the running command (a problem in 0.1.0).
- With the window on a secondary screen that uses a different scale, a new window opened with Ctrl+Shift+N appeared in the wrong place.

## 0.1.0 (2026-11-03)

The first public version, for Windows x64 (Windows 10 1809 or later), macOS (Apple silicon, macOS 11 or later) and Linux x64 (glibc 2.35 or later, such as Ubuntu 22.04 and Debian 12). The macOS and Linux versions haven't been tested on real hardware yet and don't install updates automatically (they tell you about a new version and open the download page); the macOS package is only ad-hoc signed and not notarized, so it has to be allowed under System Settings → Privacy & Security the first time.

### Connections

- SSH, Telnet and serial ports; quick connect by typing `user@address`.
- Authentication: password, private key (OpenSSH, PEM, PuTTY ppk), SSH Agent / Pageant, and interactive authentication when the server asks for it; verification codes (TOTP) filled in automatically.
- Profiles and group defaults, so many hosts can share one set of credentials.
- Multi-hop jump hosts (up to 5), SOCKS / HTTP proxies and proxy commands, agent forwarding, and support for old devices that only offer legacy algorithms.
- Host key verification; a notice when the connection stops responding, automatic reconnection, and resuming in tmux.
- After connecting, Termlet identifies the remote system (one read-only command in a separate channel, not through the terminal; skipped silently if it fails, and can be turned off under Settings → Connection) and shows the system's logo in the host list and on terminal and SFTP tabs.
- Can be opened by bastion hosts such as JumpServer and NSFOCUS OSMS.

### Terminal

- Tabs, split panes and workspaces, a command palette, and duplicate sessions (sharing the connection); a terminal tab always shows the name it was created with (the host name, the local shell and so on), with titles set by remote programs in its tooltip.
- Sync input, and a compose bar (send to many terminals, or run on many hosts and compare the output).
- Snippets (with parameters, and an optional snippet bar) and command history.
- Paste protection, sudo password filling, session logs, keyword highlighting, new-output markers on background tabs and notifications when long commands finish.
- 26 built-in terminal color schemes, set per host and per group, plus importing scheme files from Windows Terminal, iTerm2, Xshell, MobaXterm and others.
- Resource monitor and network latency; a host tools panel (processes, systemd services, Docker, network connections).
- Encoding switch (GBK, Big5), rz / sz and trzsz, pasting screenshots.
- Customizable shortcuts: Ctrl-based on Windows and Linux, with one-click Xshell and MobaXterm key presets; ⌘-based on macOS like Terminal.app and iTerm2 (the SFTP file list follows Finder), with every Control combination going to the terminal. Custom shortcuts are saved per platform, so each keeps its own after syncing. The kitty keyboard protocol.
- Local shells (PowerShell, Command Prompt, Git Bash and more, plus WSL distributions, on Windows; bash, zsh, fish and more on macOS and Linux).

### Files

- Two-pane SFTP with drag-and-drop between local and remote; a transfer queue, resuming transfers and reconnecting automatically.
- Changing permissions, following the terminal directory, editing remote files, opening SFTP as root, and remote file name encodings (GBK, Big5); the file lists can be used entirely from the keyboard.
- File management inside WSL.

### Keys and port forwarding

- Key library: import, generate (Ed25519, RSA), deploy public keys, export.
- Local, dynamic (SOCKS) and remote port forwarding, reconnecting automatically after a drop.

### Command-line tool and AI

- The `termlet` command: list hosts, run commands (also in a terminal already open in Termlet), upload and download single files.
- Off by default and allowed per host, with optional separate credentials for the command-line tool; the credentials never leave Termlet.
- When Termlet isn't open, `termlet` starts it in the background (can be turned off in Settings).
- Dangerous commands are blocked or need approval, and every call goes into an audit log.
- One-click installation of the Agent Skill (Claude Code, Codex and others).

### Data and security

- Encrypted credentials, with the master key in the system keychain (Windows Credential Manager, the macOS keychain, or the Secret Service on Linux; a master password when none is available); app lock (lock password, or Windows Hello on Windows).
- Importing hosts from `~/.ssh/config`, Xshell, FinalShell, MobaXterm, Tabby, Termora and WindTerm; exporting them as a Termlet host file or ssh_config.
- Encrypted full backups (restorable on another computer) and daily local snapshots.
- Sync between computers through a local folder or WebDAV, end-to-end encrypted and merged item by item.

### Privacy and diagnostics

- Anonymous usage statistics: on by default, sent at most once a day, with nothing that identifies this computer; turn them off any time or view the exact message to be sent under Settings → About → Privacy (see the [privacy notice](https://termlet.dev/en/privacy/)).
- Sending crash reports automatically: off by default; once on, only a scrubbed summary is sent, never memory dumps. A report the receiver rejects is not sent again and doesn't hold up later ones.
- Diagnostics (without host addresses, credentials or commands) and crash records kept on this computer.

### App

- Interface and setup wizard in 简体中文 (Simplified Chinese) / 繁體中文 (Traditional Chinese, Taiwan usage) / English; when following the system, Traditional Chinese is used on systems set to Taiwan, Hong Kong or Macao. Light / dark themes (or following the system).
- Wrapped Chinese descriptions and hints in the interface follow the usual line-breaking rules: punctuation such as full stops and commas never starts a line, opening brackets and quotation marks never end one, and dashes and ellipses aren't split across lines.
- Groups and sections in the host list start collapsed, and the ones you expand stay expanded after a restart; after you create or edit a host or group, the tree expands to it and selects it.
- System and brand logos (from Simple Icons): for identified remote systems, for Git Bash, bash, zsh, nushell and WSL distributions under This Computer, on the Docker tab of host tools, and in the command-line tool's approval dialog (when Claude Code or Gemini CLI makes the request). "Icons" (Color / Monochrome) under Settings → General applies to both file icons and these logos.
- On macOS, an app menu bar (Termlet, Shell, Edit, View, Window, Help) lists the common commands with their shortcuts; ⌘Q asks first about open connections, unfinished transfers and unsaved files.
- Windows setup wizard: installs for the current user only, with no administrator rights needed. The Linux tar.gz comes with `install.sh`, which adds Termlet to the application menu and links `termlet` into `~/.local/bin` without root.
- Automatic updates on Windows from Termlet's download site `dl.termlet.dev`, falling back to GitHub Releases (github.com/termlet/termlet), with signature verification. macOS and Linux check and verify the same way, then tell you about the new version and open the download page. Each release comes with a signed `SHA256SUMS` for checking by hand (see [Checking your download](README.en.md#checking-your-download)).
