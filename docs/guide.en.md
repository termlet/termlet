# Termlet User Guide

[简体中文](guide.md) · **English**

- This guide covers installing Termlet and the details of each feature. For an overview, see the [README](../README.en.md); for development, see the [contributing guide](../CONTRIBUTING.en.md).
- The display name is Termlet; the command, data folder and download file names use lowercase `termlet`.
- `{data folder}` below stands for the folder described under [Data folder](#data-folder).

## Contents

- [Installation](#installation): [Downloads](#downloads) · [Windows](#windows) · [Windows portable](#windows-portable) · [macOS](#macos) · [Linux](#linux) · [Data folder](#data-folder) · [Lost master key](#lost-master-key) · [Checking your download](#checking-your-download)
- [Hosts](#hosts): [Host list](#host-list) · [Identifying the remote system](#identifying-the-remote-system) · [Host dialog](#host-dialog) · [Copying the address and duplicating hosts](#copying-the-address-and-duplicating-hosts) · [Profiles and group defaults](#profiles-and-group-defaults) · [Test connection](#test-connection) · [Telnet and serial ports](#telnet-and-serial-ports) · [Importing hosts](#importing-hosts)
- [Connection and authentication](#connection-and-authentication): [Authentication](#authentication) · [OpenSSH certificates](#openssh-certificates) · [Verification codes (TOTP)](#verification-codes-totp) · [Proxy](#proxy) · [Proxy command (ProxyCommand)](#proxy-command-proxycommand) · [Jump hosts](#jump-hosts) · [Legacy devices](#legacy-devices) · [Agent forwarding](#agent-forwarding) · [X11 forwarding](#x11-forwarding) · [Host keys](#host-keys) · [Quick connect](#quick-connect) · [IPv6 and dual stack](#ipv6-and-dual-stack) · [Connection not responding](#connection-not-responding) · [Automatic reconnection](#automatic-reconnection)
- [Terminal](#terminal): [Shortcuts](#shortcuts) · [Command palette](#command-palette) · [Tab menu and right-click](#tab-menu-and-right-click) · [Paste protection](#paste-protection) · [sudo password filling](#sudo-password-filling) · [Session logs](#session-logs) · [Session recording and playback](#session-recording-and-playback) · [New output and notifications](#new-output-and-notifications) · [Pasting screenshots](#pasting-screenshots) · [Encoding](#encoding) · [Commands after login](#commands-after-login) · [Snippets](#snippets) · [Command history](#command-history) · [Command suggestions](#command-suggestions) · [Tabs, panes and workspaces](#tabs-panes-and-workspaces) · [Vertical tabs](#vertical-tabs) · [Sync input](#sync-input) · [Compose bar (broadcast input)](#compose-bar-broadcast-input) · [Keyword highlighting](#keyword-highlighting) · [Terminal color schemes](#terminal-color-schemes) · [Resource monitor and network latency](#resource-monitor-and-network-latency) · [File transfers in the terminal](#file-transfers-in-the-terminal) · [Reconnecting and closing](#reconnecting-and-closing)
- [Local terminals and WSL](#local-terminals-and-wsl): [Local shells](#local-shells) · [WSL](#wsl)
- [SFTP](#sftp): [Opening](#opening) · [Opening as root](#opening-as-root) · [Remote file name encoding](#remote-file-name-encoding) · [Two panes](#two-panes) · [Following the terminal folder](#following-the-terminal-folder) · [File operations](#file-operations) · [Keyboard](#keyboard) · [Browsing](#browsing) · [Columns](#columns) · [Changing permissions](#changing-permissions) · [Transfers](#transfers) · [Clearing and summaries](#clearing-and-summaries) · [Transfer progress on the taskbar](#transfer-progress-on-the-taskbar) · [Resuming and reconnecting](#resuming-and-reconnecting) · [Editing remote files](#editing-remote-files) · [Opening in VS Code / Zed](#opening-in-vs-code--zed) · [SCP when the server has no SFTP](#scp-when-the-server-has-no-sftp)
- [Keys](#keys): [Importing and generating keys](#importing-and-generating-keys) · [Deploying public keys](#deploying-public-keys) · [Exporting private keys](#exporting-private-keys) · [Using keys in hosts](#using-keys-in-hosts) · [Certificates](#certificates)
- [Port forwarding](#port-forwarding): [Forwarding rules](#forwarding-rules) · [Status and reconnection](#status-and-reconnection) · [Adding forwards while connected](#adding-forwards-while-connected) · [Stopping and starting automatically](#stopping-and-starting-automatically) · [Lending your local proxy](#lending-your-local-proxy)
- [Host tools](#host-tools): [Overview](#overview) · [Always-on-top overview window](#always-on-top-overview-window) · [Processes](#processes) · [Services](#services) · [Timers](#timers) · [Docker](#docker) · [Docker cleanup](#docker-cleanup) · [Log viewer](#log-viewer) · [Network](#network) · [Row actions](#row-actions) · [Details](#details) · [Jumping between pages](#jumping-between-pages) · [Keyboard](#keyboard-1) · [Confirmations](#confirmations) · [When root is needed](#when-root-is-needed) · [Audit log](#audit-log)
- [Opened by bastion hosts](#opened-by-bastion-hosts): [Setting up the bastion host](#setting-up-the-bastion-host) · [The connections it opens](#the-connections-it-opens)
- [App lock](#app-lock)
- [Backup and export](#backup-and-export): [Full backups](#full-backups) · [Restoring](#restoring) · [Local snapshots](#local-snapshots) · [Scheduled backups](#scheduled-backups) · [Exporting hosts](#exporting-hosts)
- [Sync](#sync): [What is synced](#what-is-synced) · [Sync locations](#sync-locations) · [Encryption](#encryption) · [Merging](#merging) · [Safeguards](#safeguards) · [Automatic sync](#automatic-sync)
- [Interface and settings](#interface-and-settings): [Settings pages](#settings-pages) · [Font and cursor](#font-and-cursor) · [Sidebar](#sidebar) · [Windows and the title bar](#windows-and-the-title-bar) · [Toolbar](#toolbar) · [Opening at startup](#opening-at-startup)
- [Updates, diagnostics and uninstalling](#updates-diagnostics-and-uninstalling): [Updates](#updates) · [Usage statistics](#usage-statistics) · [Diagnostics](#diagnostics) · [Logs](#logs) · [Crash records](#crash-records) · [Uninstalling](#uninstalling)
- [Command-line tool and AI](#command-line-tool-and-ai): [Turning it on](#turning-it-on) · [Agent Skill](#agent-skill) · [MCP server](#mcp-server) · [MCP over HTTP](#mcp-over-http) · [MCP over the LAN](#mcp-over-the-lan) · [Output](#output) · [Exit codes](#exit-codes) · [Uploads and downloads](#uploads-and-downloads) · [Host names and size limits](#host-names-and-size-limits) · [`exec --terminal`](#exec---terminal) · [Dangerous commands](#dangerous-commands) · [Audit](#audit) · [PATH](#path) · [Limits](#limits)
- [Known limitations](#known-limitations)

## Installation

### Downloads

| Platform | Download | Requirement |
|---|---|---|
| Windows x64 | `termlet-<version>-win-x64.exe` (setup wizard) | Windows 10 1809 (build 17763) or later |
| Windows x64 portable | `termlet-<version>-win-x64-portable.zip` (unzip and run) | Same as above |
| macOS (Apple silicon) | `termlet-<version>-mac-arm64.zip` (contains Termlet.app) | macOS 11 or later |
| Linux x64 | `termlet-<version>-linux-x64.deb`, `.rpm`, `.AppImage` or `.tar.gz` (pick one) | glibc 2.35 or later (Ubuntu 22.04, Debian 12 and newer), an X11 or Wayland desktop session |
| Windows arm64 (experimental) | `termlet-<version>-win-arm64.exe` (setup wizard), `termlet-<version>-win-arm64-portable.zip` (portable) | Windows 10 1809 or later on arm64 (Windows 11 on Arm and so on); experimental: no real hardware, tested in CI only |
| Linux arm64 (experimental) | `termlet-<version>-linux-arm64.deb`, `.rpm`, `.AppImage` or `.tar.gz` | Same as Linux x64; experimental: no real hardware, tested in CI only |

- Download from the [download page](https://termlet.dev/en/download/) of the website (mirror: [Releases](https://github.com/termlet/termlet/releases/latest) on GitHub).
- To build the packages from source yourself, see [Releasing (maintainers)](../CONTRIBUTING.en.md#releasing-maintainers).
- The macOS and Linux versions haven't been tested on real hardware yet; please report problems in [Issues](https://github.com/termlet/termlet/issues).
- The arm64 versions for Windows and Linux are experimental:
  - There is no arm64 hardware to test on, so they are only built and run through the core tests on GitHub Actions' arm64 runners.
  - The packages are the same as the x64 ones (just with arm64 programs), with `arm64` instead of `x64` in the file names, and the x64 instructions below apply to them too.
- The x64 Windows version also runs on Windows 11 on Arm through emulation and keeps updating to the x64 installer. To switch to the arm64 version, just run the arm64 setup wizard (it installs to the same place and your data is kept).

### Windows

- The setup wizard follows the system language: Simplified Chinese, Traditional Chinese, English, Japanese or Korean.
- The notice on the license page exists only in Simplified Chinese, Traditional Chinese and English; Japanese and Korean systems show the English one. The full license text after it is the English original.
- After you accept the license agreement you can change the installation folder (default `%LOCALAPPDATA%\Programs\Termlet`; upgrades keep the existing folder).
- It installs for the current user only and needs no administrator rights.
- The installation folder holds `Termlet.exe` and the command-line tool `bin\termlet.exe`, and Start menu and desktop shortcuts are created.

### Windows portable

- `termlet-<version>-win-x64-portable.zip` holds a `Termlet` folder: `Termlet.exe`, `portable.txt`, `bin\termlet.exe`, `licenses` and the license notice. Keep them where they are relative to each other.
- Unzip it into any folder you can write to (the desktop, a USB drive and so on, not Program Files) and run `Termlet.exe` inside. Nothing is installed and no administrator rights are needed.
- With `portable.txt` next to the program, Termlet runs as the portable version:
  - Data is kept in the `data` folder next to the program (logs, local snapshots, the default session log location and so on are all inside).
  - You can take the whole folder to another computer.
- The first time you open it, choose one of:
  - set a master password: saved passwords and private keys can then be unlocked on other computers too; you enter it each time you open Termlet;
  - use it on this computer only: the master key goes into this computer's Credential Manager and no master password is needed.
- Compared with the installed version:
  - It creates no shortcuts, doesn't change PATH and has no uninstall entry (just delete the folder).
  - For new versions it only notifies you and opens the download page.
  - Its `bin\termlet.exe` connects to the same portable Termlet.
- It can run alongside an installed copy: data, the single-instance lock and the command-line channel are separate.
- If you delete `portable.txt`, data goes to the system's data folder instead (existing data isn't moved).
- If the program's folder can't be written to, Termlet explains why at startup.

### macOS

- Unzip and drag Termlet.app into Applications.
- The macOS package is only ad-hoc signed (no Apple Developer certificate, not notarized), so macOS blocks it the first time. Either:
  - click "Open Anyway" under System Settings → Privacy & Security, or
  - run `xattr -dr com.apple.quarantine /Applications/Termlet.app` in Terminal and open it again.
- The command-line tool is at `Termlet.app/Contents/MacOS/bin/termlet`.

### Linux

- **deb and rpm**:
  - deb (Debian, Ubuntu and others): `sudo apt install ./termlet-<version>-linux-x64.deb`.
  - rpm (Fedora, openSUSE and others): `sudo dnf install ./termlet-<version>-linux-x64.rpm` (`sudo zypper install` on openSUSE).
  - The package manager installs the dependencies. The program goes to `/opt/termlet`, the command-line tool is linked as `/usr/bin/termlet`, and the menu entry and icons go under `/usr/share`.
  - To upgrade, download the new package and install it again.
  - `sudo apt remove termlet` or `sudo dnf remove termlet` removes everything (your data folder is kept).
- **AppImage**:
  - Make it executable (`chmod +x termlet-<version>-linux-x64.AppImage`) and run it.
  - It uses the system's FUSE (`fusermount3` or `fusermount`, which desktop distributions usually have); without it, run it with `--appimage-extract-and-run`.
  - To use the command-line tool, link it as `termlet`, for example `ln -s ~/Applications/termlet-<version>-linux-x64.AppImage ~/.local/bin/termlet`. Run under that name, it is the command-line tool, and it runs the AppImage itself when it needs to start Termlet in the background.
  - To remove it, just delete the file.
- **tar.gz**:
  - Extract the archive and run `./Termlet`.
  - The included `./install.sh` (no root needed) adds it to the current user's application menu (`~/.local/share/applications/dev.termlet.Termlet.desktop` and icons in `~/.local/share/icons`) and links the command-line tool as `~/.local/bin/termlet`.
  - The program stays in the extracted folder, so run `./install.sh` again after moving the folder.
  - `./install.sh --uninstall` removes the menu entry, icons and link.
- **Dependencies**: the AppImage and the tar.gz don't bring dependencies. At run time they use libxcb, libxkbcommon, libxkbcommon-x11 and a graphics driver (Vulkan; without it, OpenGL ES is tried). If something is missing on Debian / Ubuntu:

  ```
  sudo apt install libxcb1 libxkbcommon0 libxkbcommon-x11-0 mesa-vulkan-drivers
  ```

### Data folder

| System | Data folder |
|---|---|
| Windows | `%APPDATA%\termlet` |
| macOS | `~/Library/Application Support/termlet` |
| Linux | `~/.config/termlet` |
| Portable version | the `data` folder next to the program |

- This guide calls it `{data folder}`.
- Hosts, settings and encrypted credentials are in `data.db`.
- The master key is kept in the system keychain: Windows Credential Manager, the macOS keychain, or the Secret Service on Linux (such as GNOME Keyring or KWallet).
- When the keychain is unavailable (for example, no Secret Service on Linux), the master key is protected by a master password that you enter at every start:
  - change it under Settings → Security;
  - the master key stays the same, and only this computer is affected: sync uses your sync password, and your other computers have their own keychain or master password.
- Set the `TERMLET_DATA_DIR` environment variable to use another folder.
- Termlet restricts the data folder to the current user. If it is a drive root, your home folder, Documents, Downloads or another shared location, or holds other files, only the database, logs, backups and session logs inside it are restricted.

### Lost master key

- If the master key is lost (for example, the data folder was copied to another computer or user account), saved passwords and private keys can no longer be decrypted.
- If it is only temporarily unavailable, go back to the original environment.
- If it cannot be recovered, use "Reset vault" in the notice: hosts, groups and other data are kept, the undecryptable passwords, private keys and verification code secrets are cleared, and cloud sync is stopped.
- Then do one of these:
  - enter them again (importing the same private key into the key library again restores it);
  - set up cloud sync again (credentials at the sync location come back);
  - restore a full backup.

### Checking your download

- Each release comes with `SHA256SUMS` (the SHA-256 of that release's packages for each platform and architecture and of the release description files `latest*.yml`) and its signature `SHA256SUMS.minisig`.
- The signature uses the [minisign](https://jedisct1.github.io/minisign/) format. The public keys are listed under [Checking your download](../README.en.md#checking-your-download) in the README (the same as in [SECURITY.md](../SECURITY.md)).

Steps:

1. Install minisign:
   - Windows: `scoop install minisign` or `choco install minisign`
   - macOS: `brew install minisign`
   - Linux: your distribution's package, such as `sudo apt install minisign`
2. Put `SHA256SUMS`, `SHA256SUMS.minisig` and the downloaded package in the same folder, and check the signature there:

   ```
   minisign -Vm SHA256SUMS -P RWRSYxplUItTcPz4ZAEBJVcK0n3Sy1xPjhO3CNJIVFxbazT7cXsPacBG
   ```

   It passes only if it prints `Signature and comment signature verified` and the next line, `Trusted comment: termlet-sha256sums <version>`, names the version you downloaded.
3. Check the download against `SHA256SUMS`:

   | Where | Command | It passes when |
   |---|---|---|
   | Linux, Git Bash or WSL | `sha256sum -c SHA256SUMS --ignore-missing` | the line of the file you downloaded says `OK` |
   | macOS | `shasum -a 256 -c SHA256SUMS --ignore-missing` | the line of the file you downloaded says `OK` |
   | PowerShell | `Get-FileHash .\termlet-<version>-win-x64.exe` | the hash matches that file's line in `SHA256SUMS` (case doesn't matter) |

Notes:

- On the download site these two files are always those of the latest release; to check an earlier version, get them from that version on Releases.
- A matching SHA-256 only shows the download is complete; a valid signature is what shows the file comes from Termlet's publisher.
- If the keys are ever rotated, it will be announced in the README and in the [changelog](../CHANGELOG.en.md), and later releases are checked with the key given in the announcement.
- Build provenance (from 0.3.0 on): every release's packages are built from the version tag by this repository's GitHub Actions and come with GitHub build provenance attestations.
  - With the [GitHub CLI](https://cli.github.com/) installed, run `gh attestation verify termlet-<version>-win-x64.exe -R termlet/termlet`; when it passes, it lists the workflow and tag that built the file.
  - This shows which build of this repository a file came from, and complements the signature above.

## Hosts

### Host list

- Create, edit and delete hosts in the sidebar, with groups (nestable), tags, search and favorites.
- Collapsing:
  - Groups and the "Workspaces", "This Computer", "Favorites" and "Hosts" sections all start collapsed (so do new groups and groups that arrive through sync), and the ones you expand stay expanded after a restart.
  - Searching lists the matches directly without changing what's collapsed.
  - After you create or edit a host or group, or save a workspace, the tree expands to it and selects it.
  - The "Workspaces", "This Computer", "Favorites" and "Hosts" headings can all be collapsed (click, Space or Enter).
- Hosts that aren't in a group sit at the top level of the tree, after the groups.
- Dragging:
  - Drag hosts or groups to reorder them or move them into another group.
  - To take them out of groups, drop them next to a top-level host, on the "Hosts" heading, or on "Drop here to move out of groups" at the bottom of the list while dragging.
  - Hovering over a collapsed group expands it, and dropping into one expands it.
- Icons:
  - The icon at the start of each row is the system's logo for SSH hosts whose remote system has been identified (see [Identifying the remote system](#identifying-the-remote-system)).
  - Otherwise it shows the protocol: a server for SSH, a network for Telnet, a cable for serial, a terminal for local terminals and WSL.
  - Icons are centered vertically against the text, and icons and names line up with the groups at the same level.
- Addresses: the host list shows `user@address` and tags under each name by default (smaller and in a muted color, so the name stands out), and the icon grows to match the two lines. Turn off "Show addresses in the host list" in Settings → General.
- State: open hosts show their state at the bottom right of the icon (a solid green dot when connected, a hollow yellow ring while connecting or reconnecting, a red square when disconnected, so it reads without color too). Click it to switch to the terminal, or to reconnect when all of them are disconnected.

### Identifying the remote system

- After an SSH connection is made, Termlet runs one read-only command on it in a separate channel: `uname -s` and reading `/etc/os-release`; for Windows OpenSSH without sh it then tries `cmd.exe /d /c ver`.
- Nothing goes through the terminal and nothing is written on the server.
- Recognized systems: Ubuntu, Debian, Fedora, RHEL, CentOS, Rocky Linux, AlmaLinux, Arch, Alpine, SUSE, Gentoo, Kali, Manjaro, Raspberry Pi OS, macOS, Windows, FreeBSD, OpenBSD and NetBSD. Once one is recognized, the host list and the terminal and SFTP tabs show the system's logo, and the tooltip includes the system's name.
- Derivatives count as the upstream they declare in os-release (Linux Mint → Ubuntu, Amazon Linux → RHEL and so on).
- Linux whose distribution isn't recognized, and ones without a logo of their own (Fedora), show the generic Linux logo.
- Windows hosts (Simple Icons has no Microsoft logos) and hosts not identified yet keep the server icon.
- The result syncs to your other computers with the host but doesn't count as an edit of the host (its modification time doesn't change and it never conflicts with edits made to the host elsewhere).
- If it fails, times out or a bastion host refuses another channel, it's skipped silently without affecting the session and without an audit entry.
- Hosts that use a single channel, Telnet, serial and local terminals aren't identified.
- If you'd rather not run extra commands on servers, turn off "Identify the remote system after connecting" in Settings → Connection.
- Logos are drawn in their brand colors or in the interface's single color, set together with the file manager's file icons by "Icons" (Color / Monochrome) under Settings → General.

### Host dialog

- The host dialog has pages by purpose:
  - General: protocol, name, address, port, user, authentication, group (most hosts only need this page)
  - Connection: jump host, proxy, agent forwarding, X11 forwarding, legacy devices, single channel
  - Verification code
  - Terminal: color scheme, encoding, commands after login, tmux
  - AI & automation
  - Tags and notes
- Pages with settings show a small dot in the navigation.
- The page navigation on the left of Settings and the host dialog can be reached with Tab; ↑ ↓ switch pages and Home / End go to the first or last page.

### Copying the address and duplicating hosts

- The host's context menu (and the tab menu of terminals and SFTP) has "Copy IP address" ("Copy host name" when the address is a domain name); it copies the address only, without the port.
- "Duplicate" in the context menu creates a copy right after the host ("Name - Copy") with the same settings and saved password, private key and verification-code secret.
- Favorites, command-line tool access and its separate credentials, and port forwarding rules are not copied, so a copy pointed at another address doesn't let AI tools connect on its own, and two connected copies don't fight over the same local port.

### Profiles and group defaults

- Profiles: save a username and credential (password, private key or SSH Agent) under Profiles on the sidebar's Keys page, then choose "Profile" as a host's authentication to use it. Changing it once applies to every host that uses it.
- Profiles still in use cannot be deleted.
- Group defaults: a group (right-click → Edit group…) can set a default username, profile, jump host, proxy command and terminal color scheme. Hosts in the group and its subgroups inherit them level by level when they do not set their own.
- A host's own settings win:
  - authentication can be set to use the group's;
  - "Connect directly (no jump host)" opts out of the default jump host;
  - "No proxy" opts out of the group's proxy command.
- The host dialog shows inherited values and where they come from.

### Test connection

- While creating or editing a host, connect and authenticate once with what you entered (nothing is saved).
- It shows the time taken and the server version, or the reason it failed (authentication failed, connection refused, timeout, DNS failure, and so on).
- The first connection asks you to confirm the host key, as usual.

### Telnet and serial ports

- Choose the protocol at the top of the General page: SSH (default), Telnet or serial port.
- Telnet and serial hosts are for consoles of network devices, older systems and development boards.
- They have no SFTP, port forwarding, command-line tool or other SSH-only features. Telnet has no Verification code or AI & automation page, and serial ports have no Connection page either.
- **Telnet**:
  - Port 23 by default.
  - With a user and password filled in, Termlet answers the login and password prompts (devices that only ask for a password get the password; after a wrong password it does not retry).
  - It can connect through an SSH jump host or a proxy.
  - The session ends when the server logs out and reconnects when the network drops.
- **Serial port**:
  - Type the port or pick a detected one (shown with the device name, such as USB-SERIAL CH340).
  - Set the baud rate (with common values next to it), data bits, parity, stop bits and flow control (RTS / CTS or XON / XOFF).
  - When you pick a port, the device's USB identity is saved, so it is found again after moving to another USB socket or getting a new port number.
  - Unplugging the device reconnects automatically once it is plugged back in.
- **Terminal page**: encoding (UTF-8, GBK / GB18030, Big5), what Enter sends (CR LF / CR / LF), local echo (auto / on / off), Backspace (DEL / Ctrl+H) and the Telnet terminal type.
- **Test connection** only checks that Telnet can connect (without logging in) and that the serial port can be opened.

### Importing hosts

- Open it with More → Import Hosts… in the host list toolbar. Sources:
  - `~/.ssh/config`
  - an Xshell sessions folder
  - a FinalShell connections folder
  - MobaXterm (MobaXterm.ini, an exported `.mxtsessions` file, or bookmarks the installed version keeps in the registry)
  - Tabby (`config.yaml`)
  - Termora 2.x (data folder `~/.termora`)
  - WindTerm (its program folder, the `.wind` data folder or the `user.sessions` file in it; you enter the master password if one is set)
  - Electerm (exported bookmarks `.json`; for versions before 3.0 also its data folder)
  - Netcatty (the `.csv` exported from its hosts page)
- Preview:
  - The preview appears once you pick the source.
  - Hosts with the same address, port and user can be merged or skipped.
  - The preview can be searched and filtered by new / existing hosts, with Select all, Select none, Only new and Invert.
  - Click a row to select it and Shift+click to select a range.
- Only host information is imported, not passwords (except Termora and WindTerm, and Electerm and Netcatty when you tick the option, see below). What is not imported is noted host by host in the preview.
- **ssh_config and Tabby**:
  - `ProxyJump` in ssh_config becomes a jump host, and `ProxyCommand` and Tabby's proxy commands become proxy commands (confirmed on first connection).
  - `ForwardX11` and `ForwardX11Trusted` in ssh_config become the X11 forwarding settings, and `CertificateFile` becomes the key file's certificate file.
  - Proxies are imported, but proxy passwords need to be filled in.
  - If Tabby's config encryption is on, turn it off in Tabby first.
- **Xshell**:
  - SSH, Telnet and serial sessions, with notes, encoding and port forwards (not started automatically).
  - Proxies (names in Xshell's global proxy list), keys in its user key store, passwords and login scripts are not imported and are noted in the preview.
  - Check the settings of imported serial sessions.
- **MobaXterm**:
  - SSH, Telnet and serial sessions; bookmark folders become groups and SSH gateways become jump hosts.
  - The encoding (UTF-8, GBK, Big5) and the command run after login are imported too.
  - Tunnels (`[PortForwarding]`) are not tied to sessions and are not imported.
- **Termlet host files** (`.termlet-hosts`): see [Backup and export](#backup-and-export).
- **WindTerm**:
  - SSH, Telnet and serial sessions with groups, notes, encoding, key files, the command run after login, and the saved user name, login password and key passphrase (stored in the vault).
  - Quick bar commands become snippets.
  - Proxies, jump hosts and tunnels are not imported and are noted in the preview.
- **Termora**:
  - Only SSH hosts are imported. Folders become groups, and notes, tags, jump hosts, ssh-agent settings, login and proxy passwords are imported too.
  - Private keys from its key manager go into the key library.
  - Termora encrypts its data with a key stored in the same file, so any program on the computer can read it. The preview only marks which hosts have a password or key, and credentials never pass through the interface.
- **Electerm**:
  - Choose a bookmark file exported from Electerm (`bookmarks-….json`; an "export all data" file also has the credential profiles). The newest one in Downloads, Desktop or Documents is filled in.
  - Since 3.0 Electerm keeps bookmarks on the computer with the system's encryption (Electron safeStorage), which Termlet does not try to break, so export them first.
  - For earlier versions you can also choose the data folder (`%APPDATA%\electerm\users\default_user`, the SQLite `electerm.db` or NeDB files), where passwords are only shifted character by character.
  - SSH, Telnet and serial bookmarks are imported with groups, proxies (URL or the older form), proxy commands, private keys (key text in a bookmark is decrypted and stored in the key library, or a key file is used), credential profiles, encoding, X11, port forwards (not started automatically), the start folder and login scripts (as commands run after login).
  - Jump hosts written inside a bookmark are matched by address, port and user to hosts in the same file, and a new host is created when there is none (the preview says for whom).
  - RDP, VNC, web and other bookmarks are skipped.
- **Netcatty**:
  - The CSV from Export on Netcatty's hosts page (`hosts_export_….csv`); the newest one in Downloads, Desktop or Documents is filled in. Netcatty keeps its data on the computer with the system's encryption, so only the export can be read.
  - SSH and Telnet hosts are imported with groups, tags, notes, key file paths, proxies (HTTP, SOCKS5, proxy commands) and jump hosts (only the last hop of a chain).
  - Keys in Netcatty's key library, identities and port forwards are not in the export.
- **Saved passwords from Electerm and Netcatty**:
  - Electerm exports and Netcatty CSVs hold passwords and keys in plain text.
  - Below the preview there is "Import saved passwords", off by default (only the hosts are imported and you enter passwords when connecting).
  - When ticked, login, key and proxy passwords go into the vault and private keys into the key library.
  - On a repeated import, existing hosts only get passwords they don't have yet; saved ones are never overwritten (this also happens with "Skip").
  - Delete the exported file afterwards.

## Connection and authentication

### Authentication

- Password.
- Private key, with passphrase: OpenSSH, PEM and PuTTY ppk, from the key library, a key file or pasted text.
- SSH Agent / Pageant: on Windows, the OpenSSH Authentication Agent service first (the system's `\\.\pipe\openssh-ssh-agent` or the pipe given by `SSH_AUTH_SOCK`), and Pageant if that is unavailable or has no keys.
- To use the agent of 1Password, Bitwarden and the like, enter its pipe under Settings → Connection ("ssh-agent address").
- When the server asks for a verification code or other interactive input, a dialog asks you.

### OpenSSH certificates

- For organizations that issue short-lived certificates from an SSH CA (Vault, Teleport, Smallstep and the like), with servers that only trust the CA (`TrustedUserCAKeys`).
- Logging in with a private key presents the certificate first, and falls back to the key itself if the server rejects the certificate but has the key in `authorized_keys`.
- The certificate comes from:
  - the matching `-cert.pub` next to the key file (found automatically, as OpenSSH does);
  - a certificate file set in the host settings (`CertificateFile` is imported from ssh_config; profiles and the CLI's own credentials can set one too, and groups pass it on through profiles);
  - the certificate attached to a key in the key library;
  - the certificate attached to a pasted private key (paste the certificate line in the host settings or load it from a file; it's checked to be a user certificate on saving).
- Identities with certificates in ssh-agent are tried along with plain keys, in the order the agent lists them.
- When a certificate can't be used to log in, the reason is shown: it expired ("The certificate expired at 11-10 10:30. Get a new one issued."), isn't valid yet, doesn't list the username, was issued by a CA the server doesn't trust, or doesn't match the private key.
- When the certificate expires in less than an hour, the terminal says so once after connecting. The same goes for certificates used to log in to jump hosts, naming the jump host.
- The CLI and MCP authenticate the same way, and their results warn you the same way when a certificate is about to expire (see the CLI's [Output](#output)).

### Verification codes (TOTP)

- On the host's Verification code page, enter the secret from your authenticator app (a Base32 string or the `otpauth://` link in the QR code).
- The dialog shows the current code so you can compare it with your phone.
- Termlet then fills in the code, also when reconnecting, forwarding ports and deploying keys; the command-line tool never uses it.
- Note that the secret is stored on this computer together with the password, which reduces two-factor authentication to one factor.

### Proxy

- SOCKS5, SOCKS4 / 4a, HTTP or HTTPS, with an optional user and password.
- A host reached through a jump host does not use its own proxy (the jump host's proxy still applies).

### Proxy command (ProxyCommand)

- Like OpenSSH's ProxyCommand, it runs a command on this computer and connects through its input and output. For example:
  - `cloudflared access ssh --hostname %h`
  - `aws ssm start-session --target %n --document-name AWS-StartSSHSession --parameters portNumber=%p`
  - `ssh -W %h:%p jump`
- Placeholders:

  | Placeholder | Replaced with |
  |---|---|
  | `%h` | address |
  | `%p` | port |
  | `%r` | user |
  | `%n` | host name (for hosts imported from ssh_config, the Host alias) |
  | `%%` | percent sign |

- If the address, user or name to substitute contains spaces, quotes or shell special characters, the connection is refused (the same check as OpenSSH 9.6 and later), so they can't be interpreted as other commands.
- It starts the same way as OpenSSH:
  - On Windows it doesn't go through cmd: the first word is the program (quote paths with spaces; `.exe` can be left out) and the rest is passed to it as is. Use `cmd /c …` for pipes, environment variables and similar features.
  - On Linux and macOS it runs through your shell (`$SHELL -c "exec command"`).
- A group can set a proxy command too, used by hosts without a proxy of their own (a host can pick "No proxy" to opt out).
- It's either-or with a jump host: hosts reached through a jump host don't use it.
- **Confirmation**:
  - A proxy command runs a program on this computer, and host settings reach other computers through cloud sync and imports. So before a command first runs on each computer, Termlet shows the full command and asks you to confirm.
  - Confirmations are kept on this computer (only a hash of the command; not synced, and not overwritten when restoring a backup).
  - A changed command is asked about again, and the Connection page in Settings can make Termlet ask again for all of them.
  - The `termlet` command-line tool can't ask: for a command that hasn't been confirmed in the app it returns `proxy_command_unconfirmed`, so connect once in Termlet and allow it first.
- If the connection fails because the command exited, its exit code and the last lines of its error output are shown.
- The command ends with the connection (on Windows, together with the processes it started).

### Jump hosts

- Pick another host as the jump host, with up to 5 hops.
- Each hop uses its own credentials and confirms its own host key.

### Legacy devices

- When an old bastion host or switch only offers old algorithms such as aes128-cbc, 3des-cbc or diffie-hellman-group1-sha1, enable "Legacy devices" on the host's Connection page.
- It is off by default and applies to this host only; jump hosts follow their own settings.
- If the connection fails, the error lists the algorithms on both sides.

### Agent forwarding

- When "Forward the local ssh-agent" is enabled on the host's Connection page, `ssh` and `git` on that server can use the keys in your local agent.
- Private keys never leave this computer; note that root on the server can use your agent.

### X11 forwarding

- Enable "Forward X11" on the host's Connection page (off by default) to show the server's graphical programs on your local X server. `X11Forwarding` must be on in the server's sshd.
- You need an X server running on this computer: VcXsrv or X410 on Windows (WSLg only serves programs inside WSL; Windows programs can't reach it).
- Set the local X display on the Connection page in Settings; it applies to this computer only.
  - When empty, the `DISPLAY` environment variable is used, or `localhost:0` without it (TCP port 6000 on this computer, the default for VcXsrv and X410).
  - On Linux and macOS you can also use `:0`.
- As in OpenSSH, the server gets a fake random cookie. When a remote program connects, Termlet checks that cookie and then swaps in the real local cookie to connect to your X server, so the real cookie never leaves this computer.
- Untrusted by default (`ssh -X`, OpenSSH's upstream default): the local xauth asks the X server for a restricted cookie, so programs on the server can't read other windows or keystrokes.
  - xauth is looked up in `PATH`, and on Windows also in VcXsrv's install folder (`Program Files\VcXsrv`): VcXsrv comes with xauth, so the default works with it.
- Without a local xauth (X410, for example), nothing is forwarded, as in OpenSSH, and you need to select "Trusted" (`ssh -Y`):
  - the cookie comes from your local Xauthority, or no authentication when there is none;
  - programs on the server can read every window and keystroke on the X server, so only turn this on for servers you trust.
- If no X server is running on this computer, the connection works as usual and the terminal explains it when the first X program tries to open a window.
- The terminal also explains when the server refuses forwarding or the local xauth fails.

### Host keys

- Host keys are confirmed on first connection and recorded in the database; search and delete them under Settings → Host Keys.
- When a host key changes, the connection is refused, and a dialog explains the risk and lists the presented and recorded fingerprints. After checking with the administrator you can remove the old record and reconnect.
- Like OpenSSH, Termlet asks for the key type already recorded, so a new key type added on the server is not mistaken for a changed key.
- If the server no longer offers the recorded type and presents a different one, it is not treated as a first connection: a prominent warning says someone may be impersonating the host, and you have to check before you can "Accept anyway and connect".
- **Host certificates**:
  - Under "Trusted Host CAs" on the Host Keys page, add a CA's public key with the host patterns it applies to (like `@cert-authority` in known_hosts: comma-separated, with `*`, `?` and `!`, and `[host]:port` for ports other than 22), or import the `@cert-authority` lines from a known_hosts file.
  - Host certificates from these hosts that a trusted CA issued (still valid and listing the address you connect to) are then accepted on first connection without asking about the fingerprint, and nothing is recorded.
  - Certificates from other CAs, and expired ones or ones for another name, are checked by the host key inside them as usual.
  - Stored on this computer only and not synced.
  - Without a CA that applies, connections don't ask for host certificates, as before.

### Quick connect

- Type `user@address[:port]` in the host search box or the command palette and press Enter to open a temporary connection that is not saved to the host list.
- `ssh -p 2222 user@address`, `ssh://user@address:port` and IPv6 also work.
- A dialog asks for the password when the server wants one; keys in ssh-agent are tried first.
- Like connections opened by bastion hosts, it also accepts legacy algorithms (secure ones still come first).
- "Save to Host List…" in the tab menu keeps it as a host (with the address, port and user filled in).

### IPv6 and dual stack

- Host addresses, jump hosts and proxies can be IPv6 addresses (such as `::1`).
- When a name resolves to both IPv6 and IPv4 addresses, both are tried in parallel (Happy Eyeballs): the address your system prefers goes first, and if it hasn't connected within 250 milliseconds the other kind is tried at the same time; whichever connects first is used. If IPv6 is broken on your computer, you no longer wait for it to time out before IPv4 is tried.
- The connection timeout of 15 seconds is for the whole attempt, not 15 seconds for each address.
- The same goes for port forwarding connecting to a target on this computer, "Lend Local Proxy" connecting to your local proxy, and X11 forwarding connecting to your X server: when you write `localhost` and the other side listens on IPv4 only, there's no wait for IPv6 to fail first (about 2 seconds on Windows).

### Connection not responding

- When the network stalls (no keepalive reply for 10 seconds), the terminal shows that the connection isn't responding, with a "Reconnect now" button.
- The connection is only considered lost after about 90 seconds without replies (keepalive interval × 3, with a default interval of 30 seconds).
- If it recovers before then, the session carries on and full-screen programs like vim are not interrupted.

### Automatic reconnection

- Retries after 1, 2, 4 … 30 seconds, and immediately after the computer wakes up. It can be turned off under Settings → Connection.
- A reconnection gets a new shell, and terminal modes left by the old program (alternate screen, mouse reporting and so on) are reset first.
- To keep your work, enable "Open terminals in tmux" on the host's Terminal page:
  - each tab gets its own tmux session;
  - reconnects return to it without stopping running programs;
  - closing the tab ends the session.
- If tmux is not installed on the server, a plain shell is used and the terminal says so.

## Terminal

### Shortcuts

- In Settings → Shortcuts you can add or remove keys for each action or restore the defaults. Conflicts tell you which action uses the key, and changes apply as soon as you save.
- Windows and Linux use Ctrl:
  - Custom shortcuts need Ctrl or Alt, except F1–F24 and Shift with Insert, Delete, Home, End, Page Up or Page Down.
  - You can switch to the keys of Xshell or MobaXterm in one click, e.g. Xshell's Alt+1–9 to switch tabs, Alt+I for the compose bar, Alt+Enter for full screen and Ctrl+Alt+F to open SFTP beside the terminal, or MobaXterm's Ctrl+Alt+←/→, Ctrl+Alt+F1–F9 and Ctrl+Alt+T.
- macOS follows Terminal.app and iTerm2 and uses ⌘ (the menu bar lists the common commands with their shortcuts):
  - Every Control combination goes to the terminal (⌃C, ⌃R, ⌃W and so on reach the shell).
  - Option types special characters as the system does, unless "Use Option as Meta key" is turned on under Settings → Terminal (then it sends the ESC prefix; both Option keys work the same way).
  - Closing the last window keeps Termlet in the Dock as macOS apps do: click the Dock icon to open a window again, or press ⌘Q to quit.
  - Custom shortcuts on macOS need ⌘ or Control, and keys are shown with the ⌘ ⌥ ⇧ ⌃ symbols (symbol keys that need Shift show the character they type, e.g. ⇧⌘] shows as ⌘}).
- Custom shortcuts are saved per platform (one set shared by Windows and Linux, another for macOS), so when settings sync between computers each platform keeps its own, and changes made on Windows don't affect your Mac.
- The kitty keyboard protocol is supported, so Shift+Enter makes a new line in Claude Code and Codex.

Default shortcuts:

| Action | Windows, Linux | macOS |
|---|---|---|
| Copy / paste | Ctrl+Shift+C / Ctrl+Shift+V (or Ctrl+Insert / Shift+Insert) | ⌘C / ⌘V (⌘C does nothing when nothing is selected) |
| Select all / clear (fixed) | Ctrl+Shift+A / none | ⌘A / ⌘K |
| Command palette (search hosts, tabs and actions; quick connect) | Ctrl+Shift+P | ⇧⌘P |
| Find | Ctrl+Shift+F | ⌘F |
| Zoom in / out / reset (current tab only) | Ctrl+= / Ctrl+- / Ctrl+0, or Ctrl + mouse wheel | ⌘= / ⌘- / ⌘0, or ⌘ + mouse wheel |
| Duplicate session | Ctrl+Shift+D | ⇧⌘T |
| Compose bar | Ctrl+Shift+M | ⇧⌘M |
| Sync input (start / exit) | Ctrl+Shift+I | ⌥⌘I |
| Snippets | Ctrl+Shift+Space | ⇧⌘Space |
| Reconnect | Ctrl+Shift+R | ⇧⌘R |
| Next / previous tab (within the group, wrapping) | Ctrl+Tab / Ctrl+Shift+Tab | ⇧⌘] / ⇧⌘[ (or ⌃⇥ / ⌃⇧⇥) |
| Go to tab 1–9 | Ctrl+Alt+1–9 | ⌘1–9 |
| Open SFTP beside the terminal | None by default (Ctrl+Alt+F with the Xshell keys) | None by default |
| Zoom the interface in / out, reset the interface size (all windows; the terminal font size is unchanged) | None by default (Ctrl+= and the like belong to the terminal font size) | None by default (all three are in the View menu) |
| Move the current tab to a new window | None by default | None by default |
| Split right / down | Alt+Shift+= / Alt+Shift+- | ⌘D / ⇧⌘D |
| Move between panes (splits and the sync input grid; fixed) | Alt+arrow keys | ⌥⌘+arrow keys |
| New window | Ctrl+Shift+N | ⌘N |
| Open the default local terminal | Ctrl+Shift+T | ⌘T |
| Hide / show the sidebar | Ctrl+Shift+B | ⌃⌘S |
| Open / close host tools | Ctrl+Shift+E | ⇧⌘E |
| Open the host tools overview (focus moves to the panel for the keyboard) | Ctrl+Shift+O | ⇧⌘O |
| Full screen | F11 | ⌃⌘F |
| Lock now (when app lock is on) | Ctrl+Shift+L | ⇧⌘L |
| Close tab (fixed) | Ctrl+Shift+W | ⌘W |
| Open settings (fixed) | none | ⌘, |
| Quit, hide, minimize, close window (fixed, macOS menu bar) | the window buttons | ⌘Q, ⌘H (⌥⌘H hides others), ⌘M, ⇧⌘W |
| Terminal scrollback (fixed) | Shift+PageUp / PageDown / Home / End, Ctrl+Shift+↑ ↓ by line | The same Shift keys, plus ⌘PageUp / PageDown / Home / End and ⌘↑ ↓ by line |
| Accept a command suggestion (when turned on; fixed; without a suggestion these keys go to the terminal as usual) | → or End for all of it, Ctrl+→ for one word | → or End for all of it, ⌥→ for one word |
| Open a link | Ctrl + click | ⌘ + click |
| Column selection | Alt + drag | ⌥ + drag |

### Command palette

- Ctrl+Shift+P searches hosts, open tabs (terminals, SFTP, editors, log viewers), workspaces and actions.
- Actions include duplicate session, reconnect, new local terminal, new host and group, open SFTP on the right, snippets, save as workspace, sync input, compose bar, host tools (and their overview, which can pop out into an always-on-top window), import / export hosts, view the audit log, show port forwards, keyword highlighting, switch light / dark, switch tab position, zoom the interface in / out, change the terminal color scheme, lock now, new window, move tab to new window, settings, customize the toolbar and more.
- Start with `>` to search only actions, `@` only open tabs, `#` only hosts.
- When you type an address, the first item is a quick connect.

### Tab menu and right-click

- **Tab menu**: right-click a tab (terminal, SFTP or editor) to open the menu of that tab; "tab menu" in this guide refers to it.
  - The tab menu of terminals and SFTP connected to a server has "Copy IP address".
  - For saved hosts it has "Reveal Host in Sidebar" (shows the sidebar if it is hidden, clears the search, expands the host's groups and selects it; also in the command palette).
  - It ends with "Close", "Close Others", "Close to the Right" ("Close Below" with vertical tabs) and "Close All". The last three only close tabs in the same pane as that tab (other split panes stay open), with one confirmation for all of them.
  - Below them are "Move to New Window" and "Move to Window ›" (when other windows are open, listing the tab each of them shows); see [Tabs, panes and workspaces](#tabs-panes-and-workspaces).
- **Right-click** in the terminal: copies when text is selected, otherwise pastes. Under Settings → Terminal it can always paste, or show a menu (copy, paste, select all, find, clear, encoding).

### Paste protection

- Control characters are always removed when pasting into the terminal, so embedded escape sequences cannot end bracketed paste early or fake input.
- It asks first, letting you edit the text, for:
  - multi-line text that would run right away (when the remote has not enabled bracketed paste);
  - lines matching the dangerous command rules;
  - download-and-run commands like `curl … | sh`;
  - invisible characters (zero-width, bidirectional controls) or look-alike letters mixed into a word (such as Cyrillic а posing as Latin a).
- It can be turned off with "Check before pasting" under Settings → Terminal.
- Programs in the terminal cannot change the system clipboard through OSC 52, so sequences hidden in remote output cannot quietly replace what you copied.

### sudo password filling

- When the host has a saved password and the terminal shows a sudo (or doas) password prompt for the login user, a bar appears at the bottom.
- Press Enter to fill in the saved password (sent straight to the server, never shown in the interface), or ignore it and type the password yourself.
- su and nested ssh ask for other accounts' passwords and get no bar.
- It can be turned off under Settings → Terminal.

### Session logs

- Start or stop with "Start Session Log" / "Stop Session Log" in the tab menu, or turn on "Record session logs automatically" under Settings → Terminal.
- Logs are plain text: colors and other control sequences are removed, progress bars keep only their final state, and each line has a timestamp.
- Screens of full-screen programs like vim are not recorded.
- Each file is capped at 100MB.
- They are saved to `{data folder}/session-logs/host name/` (the folder can be changed).
- Recording tabs show a red dot.

### Session recording and playback

- **Recording**:
  - Use "Start Recording" / "Stop Recording" in the tab menu or the command palette, or turn on "Record sessions automatically" under Terminal in a host's settings.
  - A recording keeps the screen changes with their timing (colors, full-screen programs like vim and terminal resizes included) in the asciicast v2 format (`.cast`, playable with asciinema).
  - It starts with a copy of the current screen, and recording tabs show a red dot and the elapsed time.
  - Input is not recorded by default (it may contain passwords); turn on "Also record input in session recordings" under Settings → Terminal to include it.
  - Everything shown in the terminal is recorded, so check a recording before sharing it.
  - The file is completed when you stop, close the tab, or the session ends or disconnects, and the notification lets you play it right away.
  - Recordings are saved to `{data folder}/recordings/` (the folder can be changed), named after the host and the start time, up to 256MB each.
- **Playback** opens in a new tab:
  - play / pause (Space), drag the progress bar (← and → jump 5 seconds), 1× / 2× / 4× speed, skip long pauses, select and copy text, and find;
  - it uses the terminal colors saved in the recording by default (the current ones when the file has none), and "Recorded colors" in the toolbar switches to the current terminal colors;
  - jumping back resumes from keyframes saved while playing through, so even large recordings don't replay from the start.
- You can also open recordings with "Open Recording…" in the command palette or by dragging `.cast` files into the window (asciicast v1, v2 and v3).

### New output and notifications

- A terminal in the background shows a blue dot on its tab when it prints something, or a bell for the bell character, cleared when you switch to it.
- A system notification is sent if the terminal is out of sight:
  - when a command that ran over 10 seconds finishes, if the shell emits OSC 133 marks (as for command history);
  - when a program asks for a notification (OSC 9 / 777).
- Clicking the notification brings the tab back.
- Both can be turned off under Settings → Terminal.

### Pasting screenshots

- When the clipboard only holds an image, pasting in the terminal saves it as a file and types its path:
  - SSH terminals save it to `~/.cache/termlet/paste` on the server, readable only by you (the latest 20, for 7 days);
  - local terminals save it to a temporary folder on this computer (a `/mnt/` path in WSL).
- Handy for giving screenshots to AI tools on the server.
- Images up to 8MB.

### Encoding

- Choose UTF-8, GBK / GB18030 or Big5 on the host's Terminal page.
- Switch it at any time in the terminal: the encoding button in the status bar under the terminal, the tab menu, or the right-click menu.
- A switch applies to the current session only (and is kept when it reconnects).

### Commands after login

- Enter a few commands on the host's Terminal page (such as `cd /srv/app`); they run once the prompt appears.
- They run again after reconnecting to a new shell, but not when returning to the same tmux session.
- The command-line tool does not run them.

### Snippets

- Ctrl+Shift+Space (or ⚡ in the title bar, or "Snippets…" in the command palette) opens a picker that searches snippets and this host's command history:
  - Enter sends;
  - Tab only types without running (multi-line text is typed as a bracketed paste; if the terminal does not support bracketed paste, nothing is typed and you are told why);
  - Ctrl+Enter puts it in the compose bar to send to several terminals.
- Parameters: write `{{name}}` or `{{name:default}}` in a command to fill in a parameter before sending.
  - Names use letters, digits and underscores, optionally separated by spaces, dots or hyphens.
  - Go templates for docker and kubectl such as `{{.Names}}` are sent as is.
- Manage snippets under "Snippets" in the sidebar (ticking "run after sending" adds a trailing Enter), optionally in groups.
- **Snippet bar**: you can also show a snippet bar below the terminal (the button on the Snippets page or "Show Snippet Bar" in the command palette; off by default).
  - Click a button to send it to the current terminal.
  - Pick a group to show only its snippets.

### Command history

- Each line you run with Enter is recorded (also when completed with Tab or recalled with the arrow keys), encrypted on this computer, keeping the latest 1000 per host.
- Use it from the snippet picker (Shift+Delete deletes, Ctrl+S saves as a snippet).
- "Command History…" in a host's context menu or a terminal's tab menu opens that host's history to search, type into the terminal (click), run (double-click), save as snippet, delete or clear.
- Input in full-screen programs, hidden password input and commands starting with a space are not recorded.
- The command-line tool and AI cannot see the history.
- It can be turned off or cleared under Settings → Terminal.

### Command suggestions

- Off by default; turn it on under Settings → Terminal.
- While you type a command at the prompt, the latest command from this host's history that starts with what you typed is shown dimmed after the cursor.
- Accepting:
  - → or End accepts all of it, Ctrl+→ (⌥→ on macOS) accepts one word;
  - what you accept is sent to the server just like typing (to every terminal during sync input);
  - Esc goes to the terminal as usual.
- The prompt is recognized through shell integration: the server's shell emitting OSC 133 prompt marks (fish 4 and later do it by default, and Settings → Terminal has snippets for bash and zsh).
- There are no suggestions without shell integration, nor in full-screen programs, at hidden password input or while a command runs.
- Suggestions are only drawn on screen, not written into the terminal, so copying, find, session logs and recordings are unaffected.
- They disappear or update when the cursor moves away, the line changes or the terminal is resized.
- Only the command history encrypted on this computer is used (with command history off, there are no suggestions). It is not synced or uploaded, and the command-line tool and AI cannot see it.

### Tabs, panes and workspaces

- A tab shows the name it was created with (host name, local shell and so on); a title set by the program (such as `user@host: ~`) appears when you hover over the tab.
- Drag tabs to reorder them.
- Middle-click a tab to close it (the same as its ×, asking first where that would).
- When the tabs don't fit, scroll the mouse wheel over the tab bar to move along them.
- **Moving tabs to another window**:
  - Use "Move to New Window" and "Move to Window ›" in the tab menu, "Move tab to new window" in the command palette (no shortcut by default; you can assign one in Settings), or drag a tab out of the window and release it.
  - Released on another Termlet window, it moves there; anywhere else, a new window opens where you release it.
  - While you drag outside, the window shows what releasing will do. A window's only tab can only go to another window.
  - Moving disconnects nothing:
    - terminals keep their connection, screen and scrollback, running programs, session log and recording;
    - SFTP transfers in progress carry on (the session and its transfers now belong to the new window, so closing the old one doesn't affect them);
    - editors keep unsaved changes and their undo history;
    - replays keep playing.
  - Port forwards sharing the terminal's connection, and the SFTP session used by an editor moved on its own, go along too. Once no tab in the old window uses them, they belong to the new window; while one still does, they stay with the old window and are handed over when it closes, so the forwards keep running and the session stays connected.
  - Terminals and SFTP can't move while they are connecting (you'll be told why).
  - A password you typed in the old window but didn't save is asked again the next time the moved tab reconnects.
  - A window whose last tab moves away stays open and shows the start page.
- **Split panes**: the tab menu splits a tab right or down (the new pane shares the terminal's connection, no new login) or opens SFTP on the right; drag the divider to resize.
- **Workspaces**: "Save as Workspace…" remembers the layout and each pane's host; double-click it under "Workspaces" at the top of the sidebar to open all its sessions at once.
- **Duplicate session**: opens another tab for the current host over the same connection, with no password or verification code.

### Vertical tabs

- Set "Tab position" to "Left" under Settings → General (this computer only; "Switch tab position" in the command palette does the same).
- Tabs are then listed in a column beside the host sidebar (at the window's left edge when the sidebar is hidden). Panes lose their tab bar and keep only a slim title showing the current tab.
- Rows:
  - Each row shows the icon, the name (truncated when it doesn't fit; hover to see it in full) and the same status marks as horizontal tabs (connection, new output, bell, recording and so on), with a close button at the end on hover.
  - "+ New" at the bottom opens a local terminal.
- Mouse:
  - Click a row to switch to it and focus it, middle-click to close it, and right-click for the same tab menu (switching to that tab first).
  - Drag rows to reorder them, onto another pane (its rows, its heading in the list or its slim title), onto a pane's content to split, or out of the window, just like horizontal tabs.
  - Shortcuts work as usual.
- With split panes the list is grouped by pane ("Left pane", "Right pane" and so on), ◀ marks the tab each pane shows and the focused one is highlighted.
- Width and collapsing:
  - Drag the list's right edge to resize it.
  - ⇤ in its heading collapses it to icons (a dot on an icon shows the tab's most important status in the same color as in the list, and its tooltip lists them all).
  - Hovering the icons brings up the full list beside them, over the terminal, without resizing it.
  - The width and the collapsed state are remembered, and a window too narrow for the list uses the icons by itself.
- "Group tabs by host in the side list" (off by default; also in the right-click menu of the list's heading) puts tabs in the same pane that connect to the same host (terminals, SFTP, editors, logs) under a collapsible host heading with a count. It only changes the display, and collapsed hosts stay collapsed after a restart.
- Keyboard: after pressing Ctrl+Shift+J (⇧⌘J on macOS; this is "Focus tab list", changeable under Settings → Shortcuts), choosing "Focus tab list" in the command palette, or clicking an empty part of the list:
  - ↑ ↓, Home and End move;
  - Enter switches to the tab (or collapses / expands a host heading);
  - Delete closes it (asking first as usual);
  - ← → collapse and expand host headings;
  - Shift+F10 or the Menu key opens the tab's menu;
  - Esc goes back to the terminal;
  - the current row has a focus ring.

### Sync input

- Like MobaXterm's MultiExec.
- Start it with Ctrl+Shift+I, the grid button in the title bar, the tab menu or the command palette (it needs at least two connected terminals).
- The terminals in the window are tiled in a grid for the time being.
- Whatever you type in a synced terminal goes to every synced terminal, key by key: arrow keys, Tab completion and Ctrl+C all work, and you can see each terminal's completion result.
- Controls:
  - Click "Synced / Excluded" on a pane title to switch one terminal.
  - The banner on top syncs all, excludes all or inverts the choice.
  - Double-click a title (or use the maximize button) to enlarge one pane for a while.
  - When you switch to SFTP or an editor, "Back to Grid" returns.
- Pasting and dangerous commands:
  - Pastes go to every synced terminal too, with paste protection asking only once, and multi-line text that would run right away is checked for dangerous commands when pasted.
  - Before Enter, everything about to run since the last command (both what is worked out from your input and the line on each terminal's screen) is checked against the dangerous command rules, with a confirmation on a match. Cancel and nothing is sent anywhere (text already confirmed when pasted is not asked again).
  - Checks apply as long as any synced terminal is at a shell prompt.
- Not synced: the terminal's own replies (cursor position, focus and the like), mouse reporting and the wheel.
- A pasted screenshot's path goes only to the current terminal.
- At a sudo password prompt, Enter fills in each host's own saved password.
- Terminals that disconnect or reconnect are excluded and not added back automatically; terminals opened while syncing start excluded.
- On exit everything shows as before in its tabs and splits.

### Compose bar (broadcast input)

- Write a command below the terminal and send it to:
  - the current tab (the panes on screen, or the whole sync input grid);
  - the same host;
  - all terminals;
  - or the ones you pick.
- Enter sends, Shift+Enter adds a line, ↑ ↓ go through the commands you sent.
- It is checked against the dangerous command rules first and asks for confirmation on a match.
- **Run and Compare** runs it in each terminal and collects the output:
  - identical output (with the exit code) is grouped together;
  - the other groups show the lines that differ from most hosts, which makes misconfigured machines easy to spot;
  - it needs bash or sh on the remote; local Windows terminals, Telnet and serial ports are not supported.

### Keyword highlighting

- Colors error, failed, warning, ok, IPv4 and IPv6 addresses and so on.
- Only in text the program has not colored, and not in full-screen programs such as vim.
- Edit the rules (keywords or regular expressions) and colors, or turn it off, with the "Keyword highlighting" button in the title bar (the same as Settings → Highlighting).
- "Add Preset" also offers URLs, MAC addresses, UUIDs, dates and times, and numbers (with their units, such as `230ms`); presets already in the list are checked, and a default rule deleted by mistake can be added back there. Presets in the list show their name in front of the pattern.
- When rules overlap, the earlier one wins: presets are inserted in a fixed order (URLs first, numbers last), so an IP inside a URL or the numbers in a date are not colored on their own.
- `\b` and `\B` in regular expressions use ASCII word boundaries, so text right next to Chinese counts as a boundary (in "连接failed", failed is a whole word).

### Terminal color schemes

- In Settings → Color Schemes, choose the scheme used in light mode and the one used in dark mode. Any scheme can go in either; use the same one in both for a dark terminal in a light interface.
- When you switch the interface between light and dark (Settings → General or the command palette), the terminal follows.
- Programs that turn on theme change notifications (DEC 2031), such as Claude Code and Neovim, are told when the terminal background switches between light and dark and change their own colors.
- 38 schemes are built in (Solarized, Catppuccin, Tokyo Night, Gruvbox, Dracula, Nord, One Dark Pro, Ayu, VS Code and more, each with its source and license).
- Cards preview them in your font and mark colors that are hard to read on that background, and "Keep text readable" can be turned on.
- Import schemes from Windows Terminal, iTerm2 (`.itermcolors`), Xshell (`.xcs`), MobaXterm, base16, Alacritty, Kitty, Ghostty, Xresources, Tabby and Gogh (drop, choose files or paste; the format is detected), or read them directly from Windows Terminal, Xshell, MobaXterm and Tabby installed on this computer. When a search in the library finds nothing, both options are offered right there.
- Export as Windows Terminal JSON or `.itermcolors`.
- Copy a scheme as a custom one to edit its colors.
- A host's Terminal page and a group can use a fixed scheme (for example to make production stand out).
- "Change terminal color scheme…" in the command palette previews as you move through the list and can apply to the current tab only (cleared from the tab menu).
- Keyword and search highlighting follow the scheme.

### Resource monitor and network latency

- The status bar under the terminal shows network latency (green / yellow / red) and the server's CPU, memory, fullest disk (with its mount point), network speed (with small bars for the last 30 seconds) and load (load / cores).
- Numbers sit in fixed-width slots so they don't jump around as values change.
- They turn color when high: CPU, memory and disk yellow from 75% and red from 90%; load yellow at the core count and red at twice that. They only clear after dropping back a little, so they don't flicker around the threshold.
- Hover for details.
- In a narrow window the meters go first, then items by priority (latency, CPU, memory, disk, network, load) from the end.
- Choose Full or Compact under Settings → Terminal or by right-clicking the status bar. Compact shows only latency, CPU and memory, adding disk or load with a ⚠ when they run high.
- Click the status bar to open the host tools' Overview.
- It reads over the terminal's existing connection without logging in again, and not while the terminal is out of sight (unless the host's overview is popped out into a window).
- Resources need a Linux server (WSL included).
- Resources and latency can be turned off separately under Settings → Terminal.

### File transfers in the terminal

- Running `sz` / `rz` (ZModem, needs lrzsz on the server) or `tsz` / `trz` (trzsz; `trz -d` uploads folders) shows a bar at the top of the terminal to choose where to save or what to upload (or drop files onto the terminal), with progress and cancel.
- Downloads are renamed if a file or folder with the name exists, so existing files on this computer are never overwritten (even with `tsz -y` on the server).
- A failed transfer only removes what it created.
- ZModem is not supported in tmux, local terminals or WSL (you are told why and can use trzsz or SFTP instead).
- Detection can be turned off under Settings → Terminal.

### Reconnecting and closing

- **Reconnect**: a finished session reopens in the same tab when you press Enter in the terminal (or Ctrl+Shift+R); a working connection is replaced with a new one after you confirm.
- **Confirm before closing**:
  - Closing a tab or window with open terminals (remote sessions, local terminals and WSL alike) or file managers asks first (tick "Don't ask again", or turn it off under Settings → General).
  - Closing several tabs at once asks only once.
  - Unsaved remote files and transfers that would be interrupted always ask, and so does "Restart and install" for an update.
- **Minimize to the system tray when closing the window** (Windows, off by default):
  - Turn it on under Settings → General. Closing the last window then keeps Termlet running in the notification area with connections, transfers and port forwards intact, without asking.
  - Click the tray icon or "Show Termlet" in its menu to bring the window back (unlock first if Termlet is locked).
  - "Quit Termlet" in the menu works like quitting (asking first about connections, transfers and unsaved files).
  - With other windows still open, closing a window closes it as usual.

## Local terminals and WSL

### Local shells

- Local shells are detected automatically:
  - on Windows: PowerShell 7, Windows PowerShell, Command Prompt, Git Bash, MSYS2, Cygwin, Visual Studio Developer PowerShell and WSL distributions (except Docker Desktop's internal ones);
  - on macOS and Linux: your login shell (`$SHELL`) and bash, zsh, fish, nu, pwsh and others listed in `/etc/shells`, all started as login shells.
- In "This Computer" in the sidebar and on tabs, shells with a logo show it (Git Bash, bash, zsh, nushell; WSL distributions by name), and those without one, such as PowerShell and Command Prompt, use the terminal icon.
- "This Computer" in the sidebar lists the default one and those ticked as "Show under This Computer in the sidebar" under Settings → Local Terminals; double-click to open one.
- Ctrl+Shift+T or "+" in the title bar opens the default one, and the menu next to "+" lists them all.
- When you search the sidebar, the ones not shown there are listed too.
- The context menu opens, sets as default, removes from the sidebar (or shows in the sidebar) or manages local terminals.
- You can add your own local terminals in Settings (command line, start folder, environment variables).
- Split panes, the compose bar and keyword highlighting all work, and each terminal picks up the system's latest environment variables.

### WSL

- WSL tabs are titled "distribution (WSL)".
- File management in WSL looks just like SFTP. Open it with:
  - the "SFTP" button in the title bar;
  - "Open SFTP on the Right" in the tab menu;
  - the hover button and context menu of a WSL row in the sidebar ("Open File Manager", or "Open File Manager as root").
- It needs OpenSSH's sftp-server in the distribution (`sudo apt install openssh-sftp-server` on Ubuntu and Debian); without it you are told so, with the install command for that distribution.

## SFTP

### Opening

- Ways to open SFTP:
  - the SFTP button when hovering over a host, or "Open SFTP" in its context menu;
  - the "SFTP" button in the title bar (for the current terminal's host);
  - "Open SFTP on the Right" in a terminal's tab menu or the command palette.
- Opened from a terminal, it shares the terminal's connection (no new login) and follows its folder; opened from the host list, it uses a new connection.
- It closes with the window, and after a disconnect "Reconnect" returns to the same folder.

### Opening as root

- "Open SFTP as root" in a host's context menu or the SFTP tab menu starts the server's sftp-server through sudo (its location is read from sshd_config, then common locations are tried), so you can edit files only root may write, such as those under `/etc`.
- Passwordless sudo (including a rule that only allows sftp-server) opens directly.
- Otherwise you are asked for the password, and a saved login password can be used in one click. The password only goes to sudo and never into the file transfer.
- The tab and toolbar show a red root badge.
- sudo that requires a terminal (requiretty) or a one-time code, and single-channel hosts, are not supported, and you are told why.

### Remote file name encoding

- Set "SFTP file name encoding" on the host's Terminal page to GBK / GB18030 or Big5 (UTF-8 by default).
- Listing, transfers, editing, renaming and deleting all convert names with it.
- Bytes that cannot be decoded are shown as `<0xNN>` and can still be opened, renamed and deleted.
- Names the chosen encoding cannot represent are not sent to the server, and you are told why.

### Two panes

- Local on the left, remote on the right. Drag between them to upload or download (drop onto a folder to transfer into it).
- You can also drag files in from File Explorer.
- The local side offers common places and drives, and can create folders, rename, move to the Recycle Bin and open with the default program (it asks before opening programs and scripts or accessing network locations).
- When the panel is narrow (for example beside a terminal), only the remote side shows until you click "Show Local Files (Two Panes)".
- Drag the divider to resize, double-click to reset.

### Following the terminal folder

- SFTP opened from a terminal follows its `cd` (including nested shells and tmux).
- Browsing elsewhere pauses it, and "Follow the Terminal's Folder" resumes.
- This needs a Linux server that accepts `LC_*` environment variables (Debian and Ubuntu do by default), unless the shell emits OSC 7 (Settings → Terminal has the snippet).

### File operations

- Browse, create folders, rename, delete (recursively), copy paths.
- Multiple selection with Ctrl / Shift; double-click to enter a folder or edit a file.
- Copy files in File Explorer and press Ctrl+V in SFTP to upload them to the current folder.
- Ctrl+C copies the remote paths of the selected files.

### Keyboard

- The keys follow File Explorer conventions and are listed in Settings → Shortcuts.
- Moving and selecting:
  - ↑ ↓, PageUp / PageDown and Home / End move and select; Shift extends the selection.
  - Ctrl+↑ ↓ moves the current row only, and Ctrl+Space selects or deselects it.
  - The current row can rest on "..", where Enter goes up one level.
  - Type the start of a name to jump to it (pressing the same letter again cycles through the names starting with it).
- Other keys:

  | Key | Action |
  |---|---|
  | Enter | open |
  | Backspace | go up |
  | F2 | rename |
  | F5 | refresh |
  | F7 | create a folder |
  | Delete | delete |
  | Ctrl+A | select all |
  | Shift+F10 or the Menu key | open the context menu next to the current row (so upload, download, permissions and the rest work from the keyboard) |
  | Ctrl+L or Alt+D | edit the path (Esc in the path box discards the edit and returns to the list) |
  | Tab | switch between the local and remote panes |

- On macOS the keys follow Finder: ⌘A / ⌘C / ⌘V, ⇧⌘. shows hidden files, ⌘↑ goes up, ⌘↓ opens, ⌘⌫ deletes, ⇧⌘N creates a folder, ⌘R refreshes and ⌘L edits the path, while ⌥↑ ⌥↓ move the current row and ⌥Space selects or deselects it (the F keys, Enter and Backspace above work too).
- The current row has a focus ring while the list has focus.
- Going up selects the folder you came from, and after deleting, the next item in the same place is selected.

### Browsing

- Outside the root folder, the list starts with "..": double-click it to go up, or drop files onto it to put them in the parent folder.
- Files and folders get colored icons by type (scripts, configuration, archives, keys, logs, Docker, programming languages and more; folders such as .ssh and node_modules have their own).
- Set "Icons" under Settings → General to Monochrome for monochrome icons (which also applies to system and brand logos).
- Hidden files (names starting with a dot on the server, files with the hidden attribute on this computer) are shown, a little dimmed.
- The eye button on each pane or Ctrl+Alt+H switches them on or off, remembered separately for the remote and local panes.

### Columns

- Besides the name there are Modified, Size and Permissions columns.
- As a pane gets narrower, Permissions goes first, then Size, and Modified last (the name column keeps room for about ten characters), so the time shows even with two panes side by side.
- The time is always compact and the same in both panes:
  - "Nov 3 14:22" this year and "Nov 3, 2025" for earlier years (English);
  - "11-03 14:22" and "2025-11-03" in Simplified Chinese and Korean;
  - "11/03 14:22" and "2025/11/03" in Traditional Chinese and Japanese.
- Hovering shows the full local time to the second.
- Click "⋯" at the right end of the name column or right-click the header to choose the columns (shared by the remote and local panes, remembered on this computer).
- Chosen columns that don't fit the pane are noted in the menu and the tooltip.

### Changing permissions

- Right-click → Permissions…, then tick read / write / execute for owner, group and others plus SUID, SGID and sticky, or type the octal value.
- For folders you can also apply it to their files and subfolders (folders get execute permission where they have read permission, so they stay accessible).
- With several items selected:
  - bits that differ between them show as a dash;
  - only the bits you tick or untick change, and the rest keep each item's own value;
  - when applying to files and subfolders, or when a symbolic link is selected, settle the dashed bits first.

### Transfers

- Upload files or folders (or drag them in from the system) and download to a chosen local folder.
- 3 tasks run at once per session and the rest wait.
- See progress, speed and errors, cancel, or open the containing folder.
- Name clashes:
  - Uploads ask whether to overwrite or skip existing items. An overwritten file keeps its permissions; retrying an upload that failed because the name exists asks again, and "Resume All" does not overwrite such items.
  - Downloads rename on a local name clash to "name (1)".
- Both uploads and downloads are written to a `.termlet-part` file that is renamed when complete.
- Files and subfolders without read permission, symbolic links and special files (devices, named pipes, etc.) are skipped and reported separately in the list and in the summary.

### Clearing and summaries

- "Clear Finished" removes this host's finished and canceled tasks; failed ones stay and can be removed one by one or all resumed.
- "Clear when done" in the transfer list header (off by default; also in Settings → Terminal):
  - removes finished and canceled tasks 3 seconds after they end;
  - closes the success notice of rz / sz and trzsz in the terminal after 5 seconds;
  - anything with skipped items stays.
- When a batch of transfers ends, a summary pops up (with "Open folder" for downloads).
- A system notification is sent when the window is in the background and the batch took a while.

### Transfer progress on the taskbar

- Windows, macOS and Linux; on by default, and can be turned off in Settings → General (for this computer).
- **Windows**: each window's taskbar button shows the overall progress of that window's transfers.
  - All transfers are combined by bytes; it is indeterminate while folders are still being counted, and yellow when everything is waiting to reconnect.
  - It turns red when a transfer fails, until you switch back to the window.
  - It clears when everything is done.
- **macOS**: the Dock icon's badge shows the percentage for transfers in all windows, or "!" when one fails.
- **Linux**: Termlet's icon shows a progress bar for transfers in all windows and asks for attention when one fails (through the Unity LauncherEntry over D-Bus, supported by GNOME's Dash to Dock, Ubuntu Dock, KDE Plasma and others; nothing shows on desktops without it).

### Resuming and reconnecting

- Click "Resume" on a failed transfer to continue where it stopped if the source has not changed (uploads continue from what the server confirmed as written).
- Files in a folder that were already transferred are skipped.
- When the SFTP connection drops (or the SFTP service on the server exits), it reconnects automatically for up to 10 minutes, then resumes the transfers that the drop interrupted.
- If a verification code is needed or authentication fails, it stops and you reconnect yourself.

### Editing remote files

- Double-click a file or choose "Edit" to open it in an editor tab (text up to 5MB).
- Encodings:
  - UTF-8 and UTF-16 (LE, BE) are detected automatically: files with a BOM open directly (such as text redirected with `>` in Windows PowerShell 5.1, or .reg files), and files without one are recognized when it's clear;
  - a file that can't be detected can be downloaded, or opened as GBK / GB18030, Big5, UTF-16LE or UTF-16BE;
  - the status bar shows the current encoding ("UTF-16LE BOM" and so on when the file has a BOM); click it to reopen in another encoding.
- Ctrl+S saves and Ctrl+F finds and replaces (⌘S and ⌘F on macOS, where ⌘W closes the tab).
- Saving replaces the file atomically and keeps its permissions, owner, encoding, BOM and line endings.
- If the file changed on the server after you opened it, you can overwrite or reload.
- Syntax highlighting for JSON, YAML, Markdown, JS / TS (including JSX / TSX), Python, Shell, TOML, HTML and CSS (Nginx, INI, XML and Dockerfile are recognized but shown as plain text).
- The same file on the same host opens in only one editor.

### Opening in VS Code / Zed

- "Open in VS Code" and "Open in Zed" are in the remote pane's context menu (opening the selected folder, or the current folder when nothing is selected) and the SFTP tab menu.
- They appear only when the editor is installed on this computer (Settings → General can keep just one or hide both, for this computer).
- The editor connects with its own ssh (VS Code needs the Remote - SSH extension).
- If `~/.ssh/config` has a Host for this server (address, user and port), it's opened with that Host.
- Otherwise Termlet first shows:
  - where the editor will connect;
  - why connecting directly may fail (going through a Termlet jump host, proxy or proxy command, a private key stored in Termlet, a key file outside the places ssh looks by default, VS Code with a port other than 22);
  - a Host entry you can append to `~/.ssh/config` (address, user, port, the paths of the key file and of the certificate file set for it, and ProxyJump for jump hosts; no passwords, key contents or proxy commands).
- Then choose "Open directly" or "Add to config and open" (only appended to the end of the file, after backing up the original).
- WSL file management can be opened in VS Code (Zed has no link for WSL).
- Temporary connections opened by bastion hosts and hosts that use a single channel don't have these items.

### SCP when the server has no SFTP

- If the server refuses the SFTP subsystem (no sftp-server, bastion host restrictions, etc.), file management switches to SCP automatically.
- A yellow "SCP" mark shows on the toolbar, with a note above the list about what's limited.
- How it works:
  - Browsing uses read-only `ls` (both GNU and BusyBox; spaces and non-ASCII names work).
  - Creating folders, renaming and deleting use `mkdir`, `mv` and `rm`, all run through `sh -c` with paths passed as arguments.
  - Uploads, downloads and editing use the SCP protocol (`scp -t` / `scp -f` on the server).
  - Uploads still go to a temporary file that is renamed when complete, overwriting keeps the original permissions, and the modification time is set with the file.
- Limits:
  - Interrupted transfers can't continue where they stopped, and "Retry" starts over.
  - Permissions can't be changed ("Permissions…" is disabled).
  - When saving an edited file whose owner differs from yours (e.g. root editing someone else's file), the original file is overwritten in place.
  - BusyBox's `ls` shows bytes that aren't UTF-8 as `?`, so such names can't be used in SCP mode.
- If the server has no scp either, doesn't allow running commands on another channel, or the host is set to use a single channel, it doesn't open and explains why.

## Keys

### Importing and generating keys

- "Keys" on the sidebar's Keys page can:
  - import private keys (from a file or pasted; OpenSSH, PEM, PuTTY ppk v2 / v3; encrypted keys need their passphrase);
  - generate keys (Ed25519, or RSA 3072 / 4096);
  - view and copy public keys and fingerprints;
  - export public keys ("Export Public Key…" in the context menu or when viewing a public key; one line in authorized_keys format, named "key name.pub" by default);
  - deploy, export private keys, rename and delete.
- Imported keys are converted to OpenSSH format and stored encrypted, so connecting no longer needs the passphrase.

### Deploying public keys

- Like ssh-copy-id: pick one or more hosts.
- Termlet logs in with each host's saved credentials, appends the key to `~/.ssh/authorized_keys` (without duplicates), and verifies a login using only this key.
- Optionally switch those hosts to this key once verified.
- Hosts connecting for the first time ask you to confirm the host key in the window, and a password is asked for when none is saved.
- Hosts that need a verification code must have a TOTP secret saved.

### Exporting private keys

- Exporting a private key requires acknowledging the risk.
- It is saved in OpenSSH format with an optional passphrase, readable only by the current user.

### Using keys in hosts

- Choose "Key library" as the key source when editing a host to use a key.
- Several hosts can share one key.
- Keys used by hosts or profiles cannot be deleted.

### Certificates

- Below the public key, the view shows the attached OpenSSH certificate, with "Attach Certificate" (choose the certificate file or paste the line), "Replace Certificate" and "Remove Certificate".
- Only user certificates issued for this key are accepted.
- It shows the type, the issuing CA and its fingerprint, serial, key ID, validity (marked when expired or not yet valid), allowed usernames, extensions and critical options.
- Keys with a certificate are marked "certificate" in the list ("certificate expired" once it has expired).
- When a private key is imported from a file, a matching `-cert.pub` next to it that belongs to the key is attached too.
- Certificates are not secret and are synced and exported to host files along with the key.

## Port forwarding

### Forwarding rules

- Create rules under "Tunnels" in the sidebar:
  - **Local** (`ssh -L`): local address and port → target address and port as seen from the server (for example `127.0.0.1:5432` on the server itself).
  - **Dynamic** (`ssh -D`): just a local port. This computer gets a SOCKS proxy (SOCKS5, also accepting SOCKS4 / 4a; no authentication) whose connections leave from the server, with names resolved by the server.
  - **Remote** (`ssh -R`): listening address and port on the server → a target reachable from this computer. Listening on all of the server's interfaces also needs GatewayPorts on the server.
- The local address can be 127.0.0.1 (default), ::1, 0.0.0.0 or :: (for dynamic forwarding too).
  - 0.0.0.0 listens on IPv4 only and :: on IPv6 only, so you can add one rule for each on the same port to accept both.
  - With 0.0.0.0 or ::, other devices on the network can reach the target through this computer without authentication, and the interface warns you.
- Saving checks that no other rule already listens on the address.

### Status and reconnection

- The list shows status, connections and traffic.
- When the SSH connection drops it reconnects automatically, while the local port keeps listening.
- It stops and shows the reason if a verification code is needed (with no TOTP secret saved) or authentication fails.
- The title bar shows how many forwards are running; click it to jump to the Tunnels page.

### Adding forwards while connected

- "Add a Port Forward for This Host…" in a terminal's tab menu can start it at once over the terminal's connection.
- For local forwards, "Detect Server Ports" lists the ports listening on the server.

### Stopping and starting automatically

- Forwards stop when the window that started them closes.
- Rules marked "Start automatically when Termlet starts" start when the first window opens and the vault is unlocked.

### Lending your local proxy

- When the server can't reach the internet to install packages or pull images, choose "Lend Local Proxy…" in the terminal tab menu to lend your local HTTP proxy to the server (SSH remote forwarding; the server listens only on `127.0.0.1`).
- The local proxy defaults to the system proxy settings (Internet Options on Windows, network settings on macOS, otherwise variables such as `https_proxy`). If none is found, enter it yourself (e.g. Clash's `127.0.0.1:7890`; HTTP proxies only).
- The port on the server is picked by the server unless you enter a fixed one (if that one is taken, the server picks another and says so).
- Both are kept on this computer only, not synced, and can also be changed under Settings → Connection.
- Once on, the dialog shows the commands for using the proxy on the server and for undoing them, all copyable:
  - `http_proxy` and the other variables for the current shell (one click runs them in the terminal);
  - apt, dnf and yum settings;
  - a systemd drop-in for the Docker daemon (for pulling images; restarts Docker);
  - `docker build --network host --build-arg …`.
- While it's on, the terminal's status bar shows "Proxy :port"; click it to get back to the dialog and stop it.
- It's off by default and stops when this terminal's session ends (including disconnects); turn it on again after reconnecting if needed.
- If the server forbids remote forwarding (`AllowTcpForwarding no` and the like), it says so; hosts set to use a single channel can't use it.
- Other users on the server can also connect to this port and use your proxy, so stop it when you're done.

## Host tools

- The Host tools button in the title bar, Ctrl+Shift+E or the command palette opens a docked panel on the right (drag to resize). For each host it opens the page you looked at last there, or your last page for a host it hasn't seen.
- It follows the terminal on screen that you used last (in split panes, the pane you clicked last; clicking the panel, the sidebar or a dialog never switches it to another terminal) and reads over its connection (no new login, nothing is written to the server).
- Linux servers only. Not available for local terminals, Telnet and serial ports; available for WSL terminals; not available for single-channel hosts and connections opened by bastion hosts.
- When it's unavailable (no terminal, still connecting, disconnected and so on), the panel explains why and grays out its tabs. With no terminal you can "Quick connect…" or open one of your recently used hosts, and a disconnected terminal can be reconnected right there.
- Colors mean the same on every page:
  - green: running and healthy;
  - yellow: warnings and non-zero exits;
  - red: failed and unhealthy, and destructive actions such as stop, end and remove;
  - gray: incidental information such as in use, built-in and static.

### Overview

- The first page; clicking the terminal's status bar opens it too.
- System and kernel, uptime.
- CPU: overall, cores, load, iowait, steal. Per-core usage is a row of thin bars, one row even for 64 cores so an uneven load stands out, with the numbers on demand.
- The top 5 processes by CPU follow (they jump to the Processes page when clicked).
- Memory and swap.
- Network: virtual interfaces such as veth and bridges, and interfaces without traffic, are hidden until you show all.
- Disks, sorted by usage, with inode usage.
- Charts of the last 10 minutes for CPU, memory and network speed:
  - faint gridlines at 25%, 50% and 75%;
  - hover one for the time and reading at that moment;
  - nothing is sampled while the terminal is out of sight and not popped out, and those stretches stay blank, as the ⓘ next to the CPU title explains.
- Resources come from the same samples as the status bar, with no extra commands. Inodes, system information and processes are read every 10 seconds only while this page is shown.

### Always-on-top overview window

- The button at the top right of the Overview (or "Host Tools: Pop Out Overview" in the command palette) opens this host's overview in a small window of its own, titled "host · Overview".
- It shows a compact overview: small charts and current values for CPU, memory and network speed, the fullest disk and the top 3 processes by CPU.
- A larger window shows more: system and uptime, cores and load, the per-core bars, swap, every disk and 5 processes, plus PID and memory when wide enough.
- It stays on top of other windows by default; the pin in its title bar turns that off and on (Windows and macOS; Linux has no way to keep a window on top).
- The window sticks to this host instead of following the current terminal. There's one per host, and popping out again brings the existing one to the front.
- While it's open the host keeps being sampled (even with the terminal out of sight, at the status bar's interval); closing it restores the usual rule.
- While the connection is down it shows "Disconnected" with a "Reconnect" button (the same as reconnecting the terminal; passwords and such are asked in the terminal's window), and it carries on after reconnecting.
- It closes with the terminal's tab, or when the main window closes or hides to the tray.
- Its position, size and pin are remembered per host on this computer (the size at 100% interface size, so it scales with the interface size).
- The interface size and light / dark follow the main window.
- It never takes the focus when it opens or comes to the front, and it doesn't take terminal input.
- On Windows it has its own taskbar button (grouped with Termlet's windows); on macOS it's listed in Termlet's Window menu.

### Processes

- Columns: name, PID, user, state, memory and CPU (with %, where one fully busy core is 100%), plus a command line column in a wide panel (hover for all of it).
- Sortable and searchable, with details (command line, parent and child processes and more).
- **Grouping by name** (a toolbar switch, off by default): processes with the same name share one row with their count and total CPU and memory, and expand when clicked.
- **Tree** by parent (a toolbar switch; it and grouping exclude each other):
  - rows with children start with ▸ / ▾, which collapse and expand them (so do ← →);
  - sorting applies among siblings;
  - processes whose parent is not in the list, or whose PID was reused, sit at the top level;
  - when searching or filtering, the matches are listed with their parents, which are dimmed and only show where the matches are, and the branches with matches expand on their own.
- **Filtering by user**: a toolbar menu lists the current users by process count (the context menu also offers "Only processes of …"); the filter shows as a clearable chip.
- **Marks**:
  - A small icon at the start of the row marks the terminal's own shell and the programs running in it.
  - Tags after the name mark the container a process runs in ("Container web-1": the name from the Docker page's data, or from one `docker ps` read by this page, or a short ID when neither is available) and the systemd service it belongs to ("Service nginx").
  - They are read from `/proc/<pid>/cgroup` (cgroup v1 and v2; Docker, Podman, containerd and CRI-O containers).
  - In a narrow panel the tags shrink to their icons (hover for the full name) and never push the process name out.
  - A toolbar switch hides processes in containers.
- Right-click to terminate or kill; ending PID 1, sshd or this terminal's shell gets a special warning.
- Refreshing pauses while the mouse is over the list or a menu or confirmation is open, so you don't click the wrong row (the CPU readings taken just after the page opens still fill in, without re-sorting the rows).

### Services

- systemd services, split into "Custom services" and "System services" (with counts) when there are custom ones.
- Filtered by running / stopped / failed.
- Units whose unit file is missing and stopped static units are hidden by default (a chip says how many; its × or a toolbar switch shows them all).
- The info line shows the system state and "systemd 257":
  - a system state other than running (such as degraded) is a clickable warning tag that shows just the failed services;
  - in a narrow panel with no room for the version, only the state is shown rather than a cut-off version.
- In a wide panel each service takes one line (name, description, start at boot, state).
- Actions: start, stop, restart, reload, enable / disable at boot, view logs (in the log viewer) or follow them in a new tab (with sudo when the logs need passwordless sudo to be read in full).
- The details show:
  - why a service failed (the result and exit code, and when);
  - how it runs (uptime, restarts, memory, CPU time, tasks);
  - for a failed service, its last 10 log lines (in your local time zone, matching the failure time).
- When services can't be read, the reason is shown (for example, a regular user can't reach systemd over D-Bus).
- Stopping ssh or networking services warns that the connection may drop.

### Timers

- A Timers tab next to the filters lists the systemd timers (read along with the services, with `systemctl list-timers --all` and `systemctl show`, read-only).
- Each row shows the next run (with "in 22 h"), the last run, the service it triggers and its state, sorted by the next run, with a red tag when the triggered service has failed.
- Times are server times:
  - calendar expressions are interpreted in the server's time zone; the zone and its offsets are read along with the services, with the right offset on either side of a daylight saving change;
  - the info line names the server's time zone;
  - when it differs from yours, the times in the rows carry the server's offset ("next 03:00 UTC+8") and the tooltip gives your local time.
- The details show the calendar expressions (OnCalendar, saying which server time zone they use) and relative triggers (OnBootSec and so on), the server time zone, the full next and last times (with your local time when it differs), whether missed runs are caught up, the random delay and the unit file.
- The triggered service is a link (and a service's details list the timers that trigger it, linking back).
- Start, stop and enable / disable at boot ask first and handle root the same way as services, and "View logs" shows the triggered service's logs.

### Docker

- Its tab carries the Docker logo.
- The info line reads "Docker 29.3.0 · Compose 2.39.1 · 3 running" (the versions come with the list).
- Pages: containers, containers with issues, images (with creation times in your local time zone), volumes and networks.
  - Containers are grouped by compose project, whose row offers only the start / stop / restart that apply.
  - In a wide panel, containers are a table with one line per container: name, image, ports, CPU, memory, state.
- Running containers show CPU and memory, colored with the same thresholds as the status bar and sortable by CPU or memory (read with a separate `docker stats` that doesn't slow the list down).
- Non-zero exits are shown in the warning color; unhealthy, restarting and dead containers in the danger color, and the Issues page lists just those.
- A row's state reads like "Running · 15 min" or "Exited (1) · 2 h ago" (worked out from the start and finish times, so it matches the details), with failing and starting health checks in a tag of their own.
- While the page is shown it refreshes every 15 seconds, pausing while the mouse is over the list or a menu or confirmation is open. The selected container's details and recent logs are read again only when its state changes or you refresh.
- Selecting a container shows its details below:
  - image, state and uptime;
  - restart policy and count;
  - health check (with the last result);
  - ports ("host port → container port/protocol", forwardable to this computer or openable in the browser on saved SSH hosts);
  - mounts (bind mounts open in SFTP);
  - networks and IPs;
  - the last 20 log lines.
- Actions: start, stop, restart and remove (only unused images, volumes and networks can be removed), view logs, follow logs or open a shell in a new tab.

### Docker cleanup

- "Clean up…" in the toolbar first lists, read-only, what can be cleaned up and how much space it frees (`docker system df -v` and the like):
  - stopped containers;
  - unused networks;
  - unused volumes (since Docker 23 `volume prune` removes only anonymous volumes, and unused named volumes are noted as not included);
  - dangling images;
  - build cache.
- Each has a checkbox and an expandable list of names.
- When stopped containers are removed too, the networks, volumes and images used only by them count as well.
- Volumes are unchecked by default, with a prominent warning that their data is deleted permanently.
- The dialog names the host, the commands to run and the total space to free.
- After you confirm, it runs `docker container prune -f` and the rest in the order of `docker system prune` (with sudo exactly when the Docker page uses it, one audit entry each).
- It shows how many objects each removed and the space it freed, then the total.
- Podman is used when there is no Docker (without the build cache).

### Log viewer

- "View Logs" for a service or container opens a tab in the main area ("Logs · demo-app", which can be switched, split and closed). It reads over the connection of the terminal it was opened from.
- Choose what to read:
  - the last 200 / 1000 lines or all of them (only the most recent part when over 1 MB);
  - a time range (last 15 minutes, last hour, today);
  - for services, a level (warnings and above, errors and above).
- Display:
  - keywords filter the lines and are highlighted;
  - error and warning lines are colored;
  - times are shown in your local time zone (or hidden);
  - long lines wrap by default (turn it off to scroll sideways).
- In a narrow tab the toolbar tucks away the secondary items (Follow becomes an icon, Copy all and the like move under ⋯) instead of wrapping onto two lines.
- While following, new logs are read every 2 seconds (only what's new) and the view scrolls to new lines, pausing while you scroll up.
- Copy a selection or everything.
- When permissions may be hiding a service's logs (not root, no passwordless sudo and not in the systemd-journal or adm group, or journalctl reports insufficient permissions), the viewer says you may be seeing only part of them or none. It suggests adding the user to the systemd-journal group or viewing them with sudo in a terminal (the recent logs in a service's details do the same).

### Network

- Connections and listening ports (via `ss`, or `/proc/net` without it).
- Filterable (each filter tab shows its count, and the number of TIME-WAIT connections is shown when there are any) and searchable.
- When you aren't root but have passwordless sudo, it reads with `sudo -n ss`, so every process name is shown.
- On saved SSH hosts, a listening port can be:
  - forwarded to this computer with "Forward to this computer…" (which fills in the forward dialog);
  - opened with "Open in Browser": common web ports (80, 443, 3000, 5000, 8000, 8080, 8443 and so on) have a button right on the row, and any other listening TCP port has it in the context menu.
- "Open in Browser":
  - forwards the port to this computer with a forward rule over the terminal's connection (using the same local port as on the server when it's free and not below 1024, otherwise a free one, and reusing a matching rule if there is one);
  - stops when that connection ends (it shows in the sidebar's Forwards, where you can stop it);
  - then opens `http(s)://127.0.0.1:local-port` in your browser (https for 443, 8443 and the like).
- The Listening tab:
  - merges the IPv4 and IPv6 sockets of a port into one row with three columns, "Port | Listening on | Process / container" (in a narrow panel the address part is shortened, never the port);
  - ports published by a container (a Docker port mapping, or opened by docker-proxy) show the container's name and jump to it on the Docker page.
- On the other tabs:
  - the local port has a column of its own (in a narrow panel it follows the address and stays visible when the address is shortened);
  - established connections are marked ↙ inbound (the local port is a listening port) or ↗ outbound;
  - CLOSE-WAIT is shown in the warning color.

### Row actions

- Hovering over a row (or the keyboard's current row) brings up its main action at the right end (end a process, restart a running service or container or start a stopped one, remove an image, volume or network) and ⋯ (the same as the context menu), over the state text and without changing the row height.
- While an action runs, that spot shows a spinner and "Restarting…", and the row's actions (and those in its details) can't be clicked.
- When it's done it says "Restarted" for a moment.
- If it fails it says "Restart failed" for a while. The reason goes under the page's info line instead of a pop-up notice: the start of the command's output, which you can expand to the full output and copy; it stays until dismissed or the next action.

### Details

- The details of a process, service or container sit below the list.
- One header line holds the name, the state and the main actions (end, start / stop, restart and so on); the rest (logs, opening a shell, jumps, copying) is under ⋯, the same as the context menu.
- Drag the divider between the list and the details to resize them (shared by all pages, remembered at 100% interface size and scaled with it; double-click to fit the content).
- When a sub-page, filter or search hides the selected row, its details close.

### Jumping between pages

- A process's details and context menu go to its service, its container and its network connections.
- A service's details go to its main process and its connections.
- "Show Processes" in a container's details lists only the processes in that container, and each published port can show its connections (by local port).
- Process and container names on the Network page jump to them.
- The Overview's top processes jump to the Processes page.
- A jump filters by process, port or container and shows the filter as a small chip you can clear (the search box is left alone).
- When the target is no longer listed, the page says so.

### Keyboard

- The keys are listed under Settings → Shortcuts.
- Ctrl+Shift+O opens the host tools overview and moves the focus into the panel (clicking a row in a list does too).
- Ctrl+PageUp / PageDown switch pages, moving the focus to that page's list.
- In a list:

  | Key | Action |
  |---|---|
  | ↑ ↓, PageUp / PageDown, Home / End | move and select (details are read once you stop, not for every row while an arrow key is held) |
  | Enter | open the details (on the Network and Overview pages it goes to the process) |
  | ← → | collapse and expand in the process tree and in grouped processes (or go to the parent or the first child) |
  | L | view logs (services, containers) |
  | Delete | end a process, stop a service, stop or remove a container, remove an unused image and so on (asking first as usual) |
  | Shift+F10 or the Menu key | open the context menu next to the current row |
  | / or Ctrl+F | go to the search box (Enter or Esc there returns to the list and keeps the search) |
  | Esc | give the focus back to the terminal |

- While a list has the focus, the current row has a focus ring and scrolling follows it.
- On macOS: ⇧⌘O, ⇧⌘[ / ⇧⌘] to switch pages (⌃⇥ switches tabs inside the panel), ⌘⌫ to delete and ⌘F to search.

### Confirmations

- Terminating processes, starting and stopping services, starting, stopping and removing containers, and starting or stopping a whole compose project ask first.
- The confirmation names:
  - the host (name and user@address);
  - the consequence (a stopped container serves nothing until it starts again, a restart briefly interrupts it, the difference between End and Force Kill and so on);
  - the command that will run (as it appears in the audit log, with container and other IDs cut to 12 characters);
  - the containers of a compose project.
- The row says when it's done or has failed. If the panel has moved to another terminal or page, a notice does instead (failures appear at the bottom right of the window, without covering the panel's title).

### When root is needed

- Passwordless sudo is used automatically.
- Otherwise the reason and the command that would run in the terminal are shown, and "Run in terminal" runs the command in a new tab:
  - sudo asks for the password in the terminal, and you can type it or use sudo password filling (su can be used instead when the user has no sudo rights);
  - when the command finishes, host tools reload the page you started from (by the shell integration's end-of-command mark; without shell integration, once the output settles and no password is being asked, or when that tab closes).
- Passwords never go through background channels.
- Whether passwordless sudo works is checked once per connection:
  - by running `sudo -n true`, which leaves an entry in the server's auth log; skipped when you log in as root, and checked again after a reconnect;
  - if it works, the info line of the Processes, Services and Network pages ends with "sudo", otherwise with a lock icon (click it for the reason and what to do).
- Without passwordless sudo:
  - actions that need root (ending other users' processes, starting and stopping services and timers and so on) carry a terminal icon in menus and buttons;
  - their confirmation shows the command that runs in the terminal, with "Run in Terminal" as the main button (rather than trying in the background first and failing);
  - "Try Anyway" still runs it in the background (polkit or similar may allow it, and if not the reason is shown).

### Audit log

- Terminating processes, starting and stopping services and timers, starting, stopping and removing containers, Docker cleanup, and "Run in terminal" are recorded in the audit log (source "Host tools"; see [Command-line tool and AI](#command-line-tool-and-ai)).

## Opened by bastion hosts

### Setting up the bastion host

- When you click "Connect" on the web page of a bastion host such as JumpServer or NSFOCUS OSMS, it starts a local client with the command-line arguments of Xshell, PuTTY, SecureCRT, MobaXterm, WinSCP or Xftp.
- In the bastion host's local client settings, replace the path of one of these programs with Termlet (by default `%LOCALAPPDATA%\Programs\Termlet\Termlet.exe`, or wherever you installed it).
- `ssh://` links are split at the last `@` as Xshell does, so user names may contain `@` and `/`, like JumpServer's user@asset.
- **JumpServer**: in the JumpServer client's settings, choose Xshell (or SecureCRT, MobaXterm) as the SSH client and Xftp or WinSCP for file transfer, and set the program path to Termlet (the PuTTY bundled with the client cannot be changed).
- **NSFOCUS OSMS and others**: set the Xshell path in the local client's settings to Termlet (the encoding in the `.xsh` session file OSMS passes is used too).

### The connections it opens

- If Termlet is already running, a new tab opens in the existing window (WinSCP and Xftp style requests open the file manager).
- By default it asks first each time. "Opened by bastion hosts and other programs" under Settings → Connection can open directly or ignore such requests.
- It opens a temporary connection:
  - not saved in the host list;
  - the password stays only in memory;
  - the host key is confirmed as usual;
  - legacy algorithms used by old bastion hosts are accepted (secure ones still come first);
  - without a password, keys in ssh-agent are tried first.
- It uses a single channel (many bastion hosts close the session when a second channel opens on the same connection), so there is no resource monitor, host tools, duplicate session, split pane, SFTP beside it or command history.
- "Save to Host List…" in the tab menu keeps it as a host (with the address, port, user and password filled in).
- For a bastion host saved in the host list, check "Use a single channel (bastion host)" on the host's Connection page for the same protection (duplicating a session opens a new connection instead).
- When a server refuses extra channels, Termlet also remembers it and stops trying (except when the connection merely reached the server's session limit, MaxSessions; then it retries later).
- JumpServer's connection tokens are single-use, so after a disconnect go back to the web page and click "Connect" again.

## App lock

- Under Settings → Security, click "Enable app lock…" and set a lock password (at least 6 characters).
  - Termlet can lock at startup, after this computer has had no keyboard or mouse input for a while (15 minutes by default, or never) and when the system locks.
  - Press Ctrl+Shift+L, or use the lock button in the title bar or the command palette, to lock right away.
- When locked, the whole window becomes a lock screen and the workspace's actions and shortcuts stop working until you enter the lock password, or use Windows Hello on supported devices.
  - Open sessions, transfers and port forwards keep running, and unlocking returns you to where you were.
  - While locked, the command-line tool returns exit code 6, and requests from bastion hosts are handled after unlocking.
- The app lock keeps others from using this logged-in computer while you are away; it does not defend against programs running on this computer as you.

## Backup and export

### Full backups

- Under Settings → Backup, click "Create Backup…", set a backup password (at least 8 characters) and save a `.termlet-backup` file.
- It contains hosts, groups, credentials, keys, profiles, snippets, port forwards, workspaces, color schemes, settings, host keys and command history, and optionally the audit log.
- The app lock password is not included.
- The whole file is encrypted with the backup password (Argon2id + AES-256-GCM) and does not depend on this computer's keychain, so it can be restored on another computer or after reinstalling.
- It cannot be opened without the password.

### Restoring

- Click "Restore from Backup…", choose the file and enter the password.
- After you check the backup time and the number of hosts and other items, all current data is replaced.
- Open terminals are not affected, and running port forwards stop.
- A snapshot of the current data is saved first, so you can roll back.
- If this computer's master key is lost, the vault is reset before restoring.
- Backups from a newer version of Termlet require upgrading first.

### Local snapshots

- One is saved automatically every day (not again when nothing has changed). The latest 7 are kept by default; you can turn this off or change the number in Settings.
- One is also saved before restoring, importing hosts, syncing and upgrading to a new version.
- Save one by hand with "Save Snapshot Now".
- Snapshots are in the `backups` folder of the data directory. Their credentials are still encrypted with this computer's master key, so they can only be restored here.
- Pick one under Settings → Backup and click "Restore…" to roll back.

### Scheduled backups

- Under Settings → Backup, click "Set Up Scheduled Backups…" and choose:
  - a folder (which can be a cloud drive's synced folder);
  - daily or a day of the week;
  - a time (03:00 by default);
  - how many to keep (1–100, 7 by default);
  - a backup password.
- Full backups are then created on schedule (the same as manual ones, encrypted with the backup password) and named `termlet-auto-date-time.termlet-backup`.
- When they run:
  - on time while Termlet is running;
  - a backup missed while Termlet wasn't running (shut down, asleep) is made once about 2 minutes after the next start, however many were missed;
  - while the vault is locked, it waits until it's unlocked.
- Only old backups named this way are removed beyond the number to keep, never manual backups or other files in the folder.
- The page shows the next time and the last result (the file name, or why it failed, such as a missing folder), and "Back Up Now" runs one right away.
- The backup password is kept on this computer, encrypted by the vault. It isn't synced or included in backups, and must be entered again after the vault is reset.

### Exporting hosts

- More → Export Hosts… in the host list toolbar (or "Export…" in a host's or group's context menu); select groups or hosts and export them in one of these formats.
- **Termlet host file** (`.termlet-hosts`):
  - jump hosts, profiles, library keys and group defaults that the hosts use are exported with them, and "Include port forwards" is optional;
  - by default passwords and private keys are not included, and the file is plain JSON you can read and edit;
  - with "Include passwords, private keys and other credentials" you must set a password, and the whole file is encrypted;
  - on another computer, use "Import Hosts" with "Termlet host file (.termlet-hosts)" (encrypted files ask for the password); you can preview it and skip or merge existing hosts as usual;
  - imported hosts can't be used by AI tools and scripts;
  - the preview marks proxy commands (run on this computer when connecting) and commands after login (run on the server), so make sure a file from someone else comes from a trusted source.
- **OpenSSH config** (ssh_config):
  - the address, port, user, key file (and the certificate file given for it, `CertificateFile`), jump host (`ProxyJump`), proxy command (`ProxyCommand`, including one inherited from the group), X11 forwarding (`ForwardX11`, `ForwardX11Trusted`) and port forwards of SSH hosts;
  - without passwords, ready for the `ssh` command;
  - Telnet and serial hosts are not exported.

## Sync

### What is synced

- Synced: hosts, groups, credentials (passwords, private keys, TOTP secrets), the key library, profiles, snippets, port forwards, workspaces, color schemes, and settings such as language, theme, terminal, keyword highlighting, connection, shortcuts and dangerous-command rules.
- Not synced: settings that belong to this computer (local terminals, app lock, interface size, the command-line tool switch, local snapshots, the session log and recordings folders, whether input is recorded and so on), command history, the audit log and host keys.

### Sync locations

- Under Settings → Sync, choose a sync location:

  | Location | What to enter | Notes |
  |---|---|---|
  | Local folder | the folder | for example a folder synced by OneDrive, Nutstore or iCloud, which carries it between computers, or a USB drive |
  | WebDAV | the address, user name and password | for Nutstore use `https://dav.jianguoyun.com/dav/<folder>/` with an app password; Nextcloud, Synology and others work too |
  | S3-compatible storage | endpoint, region, bucket, path prefix, access key ID and secret access key | check "Use path-style addresses" for MinIO and similar, and use the region `auto` for Cloudflare R2 |
  | GitHub Gist | a personal access token | a classic token with the `gist` scope, or a fine-grained token with read and write access to Gists; leave the Gist empty to find Termlet's Gist in the account, or create a secret Gist if there is none |

- Optionally "Test connection", then set a sync password and click "Start syncing".
- On other computers, choose the same location and enter the same sync password.
- WebDAV and S3 addresses must use `https://` by default:
  - for a NAS or MinIO on your local network over `http://`, check "Allow insecure HTTP" (the user name and password or the requests can then be seen or tampered with by others on the same network, while the synced data stays encrypted);
  - this choice belongs to this computer only.
- The WebDAV password, the S3 secret access key and the GitHub token are kept on this computer, encrypted by the vault; they are not synced or written to logs.

### Encryption

- The location holds a single file, encrypted as a whole with the sync password before it is written (Argon2id + AES-256-GCM), so nothing, not even host names or addresses, can be read there.
- Sync never goes through Termlet's servers.
- The sync password cannot be recovered if forgotten.

### Merging

- Items are merged one by one:
  - a change made on one side is taken;
  - when both sides changed the same item, the newer change wins and is listed under Sync (where you can switch to the other side);
  - when one side deleted an item the other changed, the change is kept.
- Before uploading, Termlet checks that no other computer wrote in the meantime and merges again if one did, so nothing is overwritten wholesale.
- WebDAV and S3 use the server's conditional writes; S3 services without them and Gists are read again and compared right before uploading.

### Safeguards

- On the first sync, if both sides have data, you choose to merge, use the sync location, or use this computer.
- Syncs that would delete many items ask first.
- A snapshot of this computer is saved before applying other computers' changes, so you can roll back under Backup.
- Nothing is written when the sync password is wrong.
- If the sync folder is missing (USB drive not plugged in, cloud drive not mounted), nothing is synced.
- If the sync file is missing from the sync location (deleted, or not yet downloaded by the cloud drive), nothing on this computer is deleted and this computer's data is uploaded again.
- If the cloud drive replaced this computer's upload with another computer's older file, deletions in it are not applied and this computer's changes are uploaded again.
- When a host's modification wins over a deletion, the credentials, keys and forwarding rules it uses are kept too.

### Automatic sync

- On by default: one minute after startup, about 20 seconds after changes on this computer, and every 10 minutes (configurable).
- A sync icon appears on the right of the title bar while syncing, after an error, or when your choice is needed; click it to open the sync settings.

## Interface and settings

### Settings pages

| Page | Contents |
|---|---|
| General | language, appearance (light / dark / follow the system), icons (color / monochrome, for file icons and system and brand logos), interface size, confirm before closing, show addresses in the host list, minimize to the system tray when closing the window (Windows), transfer progress on the taskbar (Windows, macOS, Linux), opening remote folders in an editor, open at startup |
| Toolbar | — |
| Terminal | font size (13 by default), scrollback, font, copy on select, right-click behavior, cursor style and blinking, resource usage and network latency, status bar (full / compact), check before pasting, new-output markers and notifications, file transfer detection and clearing finished transfers, sudo password filling, session logs, session recording (recording input, the recordings folder), command history, shell integration snippets, command suggestions |
| Color Schemes | — |
| Local Terminals | — |
| Highlighting | — |
| Connection | keepalive interval, automatic reconnection, identifying the remote system after connecting, ssh-agent address, opening by bastion hosts and other programs |
| Host Keys | — |
| Security | app lock |
| AI & automation | external CLI, dangerous command rules, the termlet command and PATH, Agent Skill, MCP server configuration and HTTP, audit log |
| Backup | full backups and restore, local snapshots |
| Sync | — |
| Shortcuts | — |
| About | version with "Copy version info" (version, operating system and architecture, for problem reports), website, changelog, send feedback, license agreement, third-party licenses, data folder, updates, privacy (usage statistics and crash reports), diagnostics |

- Interface languages: Simplified Chinese, Traditional Chinese, English, Japanese and Korean (the Japanese and Korean translations are the developer's own; corrections are welcome).
- Interface size (under General; use system setting / 90%–150%, for this computer):
  - text, buttons, panels and dialogs scale together in every window as soon as you save;
  - "Use system setting" matches Text size in Windows Settings > Accessibility, and is 100% on macOS and Linux;
  - the sidebar and host tools panel widths scale too; the terminal font size doesn't.
- Changes apply when you click "Save".

### Font and cursor

- The font defaults to the bundled JetBrains Mono. You can pick and preview any installed monospace font, and recommended fonts worth installing are listed too.
- The cursor is a steady block by default; under Settings → Terminal you can make it a bar or an underline and turn on blinking.
- Cursor styles and blinking set by programs such as vim (DECSCUSR) take precedence, and a terminal reset goes back to your setting.
- The cursor blinks only while the terminal has focus and its window is in front and visible, and stays solid for a moment after output or typing.

### Sidebar

- The sidebar has four pages: Hosts, Snippets, Keys and Tunnels.
- Drag its right edge to resize (or click it and use the arrow keys; double-click to reset).
- Hide it with Ctrl+Shift+B or the title bar button.
- Its width, visibility and expanded groups are remembered on this computer.

### Windows and the title bar

- The main window remembers its position, size and whether it was maximized (saved when it closes; with several windows the last one closed wins; full screen counts as maximized), and reopens on the same screen.
- If that screen is gone or the title bar would be off screen, it goes back to the middle of the main screen, never larger than the screen.
- The first time it opens at 1280 × 800 (smaller if the screen can't fit that).
- Title bar:
  - on the left: hide / show the sidebar, and open a local terminal ("+", with a menu next to it listing all local terminals);
  - on the right: a toolbar that by default holds, when relevant, host tools, sync input, the number of running forwards (click to jump to the Tunnels page), "SFTP" for the current terminal and snippets (⚡), then keyword highlighting, switch light / dark and lock now (when app lock is on);
  - at the far right, always: the notice that a new version has been downloaded, the sync status and Settings.
- Ctrl+Shift+N or "New window" in the command palette opens another window.

### Toolbar

- The toolbar can be customized (for this computer, not synced):
  - under Settings > Toolbar, pick which buttons to show, move them up or down, or restore the defaults;
  - or right-click a title bar button for "Hide this button" and "Customize Toolbar…".
- A Compose bar button is available but hidden by default.
- Hidden buttons stay available from the command palette and their shortcuts.
- When the window is narrow or the interface is enlarged, buttons that don't fit move into the More (⋯) menu.

### Opening at startup

- Under Settings > General (for this computer), choose nothing, the default local terminal, or one or more hosts (opened in the order listed, then the first one is shown).
- They open only when Termlet starts, not in new windows.
- They open after the vault and app lock are unlocked, and before connections opened by other programs.
- Hosts that are already open aren't opened again.
- Termlet doesn't restore the previous tabs, only the window position and size and interface state such as the sidebar.
- Hosts in the list that were deleted are skipped at startup, reported once and removed from the list.

## Updates, diagnostics and uninstalling

### Updates

- With "Check for updates automatically" on under Settings → About, Termlet checks at startup; you can also click "Check for updates".
- **Installed version**: it downloads the new installer in the background.
  - While it downloads, the title bar shows a download icon with the progress: hover for the version and size, click for the progress, downloaded / total size, speed and time left (About shows the size and speed next to its progress bar too).
  - Then the title bar says a new version has been downloaded. Click it (or "Restart and install" under About) to restart and install: once Termlet closes, an "Updating Termlet" progress window appears, then closes and reopens Termlet by itself (usually within a few seconds), so there's no need to open it again; otherwise it installs quietly when you quit.
  - The x64 and arm64 installed versions on Windows each download the installer for their own architecture (each has its own signed version file: `latest.yml` for x64, `latest-arm64.yml` for arm64).
- **Announce only**: development builds, the portable version and other copies run without installing, and the macOS and Linux versions only announce the new version.
  - The title bar shows "Version X available". Clicking it or "Download" under About opens the download page of the website at the section for your kind of package and architecture (portable, macOS, or the Linux deb, rpm, AppImage or tar.gz, with separate sections for arm64; the Linux section when Termlet can't tell).
  - You replace the app yourself. macOS and Linux read their own signed version files, `latest-mac.yml` and `latest-linux.yml`, or `latest-linux-arm64.yml` on Linux arm64; on Linux, if you installed the deb or rpm, install the new package.
- With "Check for updates automatically" off, the title bar doesn't announce new versions or download progress on its own.
- Termlet checks the download site `dl.termlet.dev` first and falls back to GitHub Releases when it can't be reached.
- Signatures:
  - update files carry a signature made at release time with an offline private key (minisign format);
  - Termlet verifies it with its built-in public keys before downloading, and checks the installer again before installing;
  - if the signature is wrong, the update stops and Termlet suggests downloading manually.
- The `TERMLET_UPDATE_URL=<address>` environment variable sets the update source, and `off` disables checks.

### Usage statistics

- On by default; anonymous, at most once a day (UTC).
- It contains the version, the operating system and its major version (such as Windows 11 25H2, without the build number), CPU architecture, interface language, a band for how long Termlet has been in use and two flags for "first this week / this month".
- No hosts, commands or files, and no device ID or anything else that identifies this computer.
- The server also works out the country or region from the IP address, doesn't keep the address itself and stores only daily counts.
- This computer keeps only two dates (`{data folder}/usage.json`: the first and the last message).
- There is no pop-up about it; it is stated in the license agreement and the privacy policy on the website.
- Turn it off any time under Settings → About → Privacy (which also deletes `usage.json`); "View what is sent…" shows the exact message.
- Each message sent is also written to the log.
- The environment variables `TERMLET_USAGE_URL=off` or `DO_NOT_TRACK=1` turn it off too; the `termlet` command-line tool sends nothing itself.
- To prevent abuse, Cloudflare rate-limits requests by network address; blocked requests may briefly appear with their address in Cloudflare's security logs. Termlet itself does not read or store addresses.

### Diagnostics

- Under Settings → About, "Copy diagnostics" or "Export diagnostics…" (saved as a text file).
- It gives the version, environment, a settings summary, item counts and the end of the logs (logs are in `{data folder}/logs`).
- It leaves out host addresses, credentials, command contents and the audit log.
- Attach it when reporting a problem.

### Logs

- `{data folder}/logs/termlet.<date>.log`, one file per day, kept for 14 days.
- When troubleshooting, set "Log detail" under Settings → About to "Detailed" (every step) or "Full" (also SSH and SFTP protocol details). It takes effect as soon as you save, so switch back to "Normal" when you're done.
- You can also set it at startup: `Termlet.exe --log-level detailed` (or `--verbose`), `--log-level full`, or a `tracing` filter (such as `--log-level "info,termlet_core::ssh=trace"`).
- The environment variable `TERMLET_LOG` takes the same values and wins over the option.
- When set at startup, the setting takes effect the next time Termlet starts normally.
- Passwords, private keys, verification codes and commands are never logged.

### Crash records

- Crash information stays on this computer by default (in the log folder, keeping the latest 20).
- The next start tells you the previous run had an error, and "Crash records…" under About shows them.
- "Send crash reports automatically" (off by default) only sends a scrubbed summary, never memory dumps or logs.
- The switch is under About → Privacy, and reports go to `api.termlet.dev` (the address comes from the signed remote configuration, and the switch is hidden without it).
- Reports the server rejects are marked "Rejected by the server", are not sent again and do not hold up later reports.

### Uninstalling

| Version | How to uninstall |
|---|---|
| Windows (installed) | the uninstaller removes the termlet command from PATH and deletes the shortcuts and the update cache |
| Windows portable | delete the whole folder (its data is in the `data` folder inside, so back it up first if you want to keep it) |
| macOS | delete Termlet.app |
| Linux deb or rpm | remove it with the package manager (`sudo apt remove termlet`, `sudo dnf remove termlet`) |
| Linux AppImage | delete the AppImage file |
| Linux tar.gz | run `./install.sh --uninstall`, then delete the extracted folder |

- Your data folder is always kept.

## Command-line tool and AI

```bash
termlet ls [-q web]                                # list hosts that allow CLI access
termlet exec <host> "uptime"                        # run a command; its exit code is passed through
termlet exec <host> --stdin < script.sh             # read the command from stdin (avoids shell escaping)
termlet exec --terminal <host> "kubectl get pods"   # run it in this host's terminal already open in Termlet
termlet upload <host> ./app.tar.gz /tmp/            # upload one file (scp semantics)
termlet download <host> /var/log/syslog .           # download one file (scp semantics)
termlet skill install [--agent claude|agents|workbuddy | --dir <folder>]  # install the Agent Skill (Claude Code by default)
termlet skill                                      # print the skill (SKILL.md)
termlet mcp                                        # MCP server (started by AI tools from their configuration, see below)
termlet version
```

### Turning it on

- The external CLI is off by default; tick "Allow access from AI tools and scripts" under Settings → AI & automation (it covers both the termlet command and the MCP server). While it is off, every request returns `cli_disabled` (exit code 7).
- Each host also needs "Allow AI tools and scripts to use this host" when editing it (on the AI & automation page).
- You can additionally tick "Use a separate credential for AI tools and scripts" to give the CLI its own user and password / key. A low-privilege account on the server is recommended; the real permission boundary is on the server.
- If Termlet is not running, `termlet` starts it in the background by default (no window; it quits after 10 minutes without requests).
- Untick "Also works when Termlet isn't open (start it in the background)" under Settings → AI & automation to use the CLI only while Termlet is open. It then returns `app_not_running` (exit code 7) and asks you to open Termlet first.
- Termlet quits once all its windows are closed (after any commands still running finish).

### Agent Skill

- Under Settings → AI & automation, install it with one click to:
  - the shared folder (`~/.agents/skills`, read by Codex, Gemini CLI, Cursor, GitHub Copilot, Pi and others);
  - Claude Code (`~/.claude/skills`);
  - or WorkBuddy.
- Or install it from the command line: `termlet skill install --agent agents` installs into the shared folder (`--agent codex` and `--agent pi` do the same).
- You can update or remove it there; "Copy skill" copies the whole file for other AI tools.
- AI tools then use termlet instead of calling ssh directly.

### MCP server

- AI tools such as Claude Code, Codex and Cursor can call Termlet directly as an MCP server (list hosts, run commands, upload and download files). There are two transports:
  - **Standard input and output** (recommended): the AI tool starts `termlet mcp` itself from its configuration, and no network port is opened. If Termlet isn't running, it's started in the background as set under [Turning it on](#turning-it-on).
  - **HTTP**: Termlet listens on this computer and the AI tool connects with an address and a token. It only works while Termlet is running; see [MCP over HTTP](#mcp-over-http).
- Which to choose: standard input and output usually, with no port or token to look after. Use HTTP when the AI tool only supports HTTP MCP servers, when it runs on a server and connects back to this computer through SSH port forwarding, or when it's on another computer on your LAN. Claude Desktop can only use standard input and output.
- Both transports have the same tools, checks, approval and audit. You only need one of them in an AI tool (both are named `termlet`).
- Copy every AI tool's configuration from the settings page: under Settings → AI & automation → MCP server, pick the transport and the AI tool (Claude Code, Codex, Cursor, VS Code (GitHub Copilot), Gemini CLI, Claude Desktop, or generic JSON for other tools) to get the configuration and where it goes, and copy it with one click.
  - For standard input and output, the command is the full path of termlet on this computer; for HTTP, the address and token are filled in (the preview masks the token; what you copy contains it).
  - For Claude Code, Codex and Gemini CLI there's both an `mcp add` command and the configuration file (for Codex over HTTP, only the configuration file).
- You only need one of MCP and the Agent Skill (MCP needs no terminal, so tools like Claude Desktop that can't run commands can use it too).
- The tools match the commands:

  | MCP tool | Command |
  |---|---|
  | `list_hosts` | `ls` |
  | `run_command` | `exec` (its `terminal` argument is `--terminal`) |
  | `upload_file` | `upload` |
  | `download_file` | `download` |

- Local paths for `upload_file` and `download_file` must be absolute (paths on the computer running Termlet).
- Every call goes through Termlet as usual: the external CLI switch, per-host access, approval of dangerous commands, the audit log and credentials staying in Termlet all work as for the command-line tool.
- The audit log's Source column shows "MCP (client name)", "MCP·HTTP (client name)" over HTTP, or "MCP·LAN <its address> (client name)" from another computer on the LAN, and you can filter for these calls. The caller starts with `mcp:<client name>`; over HTTP it's followed by `http` and the chain of processes that opened the connection (recorded on Windows and Linux), or `lan:<its address>` from the LAN.
- The approval dialog for a dangerous command names the AI tool in its title, such as "Claude Code (MCP) wants to run a dangerous command on …": common AI tools get their product name, others the name they report; those without a brand logo get a generic AI icon.
- When the external CLI is off, Termlet isn't running, or a command is blocked or denied, the tool returns an error with the same error code as the JSON output.
- Commands return at most 32 KB of stdout and stderr together to the AI (the beginning and end are kept, with a note on how to get the full output).
- While a call runs, Termlet reports progress every 15 seconds when the AI tool asks for it (Claude Code and Codex both do). Some AI tools abort a call after a while without progress, so longer commands aren't cut off for that reason.
- When the AI tool cancels a call (or drops the HTTP connection), Termlet stops the request (the remote command gets SIGTERM), and the audit log records it as "Canceled".
- Codex:
  - `tool_timeout_sec = 900` in the configuration is the longest a single command may run, including the time you take to approve a dangerous command; past it, Codex aborts the call. Codex defaults to 60 seconds, and progress reports don't extend it. Adding the server with `codex mcp add` doesn't set it, so add it yourself under `[mcp_servers.termlet]` in `config.toml`; raise it for commands that run longer.
  - By default, Codex asks you before every command, upload and download (listing hosts isn't asked). To skip that, add `default_tools_approval_mode = "approve"` under `[mcp_servers.termlet]`. Termlet still only lets AI use the hosts you allow and still asks you to approve dangerous commands. You need it where nobody can answer, such as `codex exec`, or those tools fail.
  - With a system proxy on (such as Clash), Codex also sends requests for `127.0.0.1` through it, ignoring the proxy's bypass list: the token passes through the proxy, and if the proxy isn't on this computer, HTTP can't connect at all. Set the environment variable `NO_PROXY=127.0.0.1,localhost` before starting Codex.
- It supports MCP revision 2026-07-28 and the `initialize`-based 2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05.

### MCP over HTTP

- Turning it on: under Settings → AI & automation → MCP server, set the transport to "HTTP", tick "Listen for HTTP" and save.
  - It's off by default. "Allow access from AI tools and scripts" at the top must be on as well; Termlet only listens when both are on, and opens no port otherwise.
  - "Listen on" defaults to "This computer only"; for "This computer and LAN", see [MCP over the LAN](#mcp-over-the-lan).
  - "Status" shows what's actually happening: running, off, or why it couldn't listen.
- Address: `http://127.0.0.1:<port>/mcp`. The port defaults to 47800 and can be changed (1–65535); change the address in your AI tools along with it.
  - That's the only address, and it only accepts POST. In the AI tool, choose the HTTP type (some call it Streamable HTTP or `streamableHttp`); the old SSE type isn't supported.
  - If the port is in use by another program or reserved by the system (Hyper-V and WSL reserve some port ranges), "Status" says so and Termlet doesn't switch ports by itself: choose another port and save, or close that program and click "Retry".
- Token: the AI tool sends it in the `Authorization: Bearer <token>` header; the configuration from the settings page already has it.
  - It's generated the first time you turn HTTP on and save (after unlocking, if you use a master password) and starts with `termlet_mcp_`.
  - The settings page masks it; you can show and copy it. With a master password, you need to unlock the vault to view it while it's locked (checking the token still works).
  - Termlet checks requests against a digest of the token only; the token itself is kept encrypted by the vault, just so you can view and copy it. The token stays on this computer: it isn't synced or included in backups.
  - "Regenerate…" takes effect right away: AI tools using the old token are refused, so copy the new configuration into them. Regenerate it if the token leaks (for example, if it was committed with your code).
  - With a wrong token, the AI tool gets a 401. Keep configurations with the token in user-level configuration files only, not in a project (`.mcp.json`, `.cursor/mcp.json` and so on), where they could be committed with your code; the settings page always gives the user-level form.
- It only works while Termlet is running: HTTP is served by Termlet itself, so AI tools can't connect after Termlet quits, and unlike standard input and output, nothing starts Termlet in the background.
  - On Windows, to keep it available after you close the window, turn on "Minimize to the system tray when closing the window" under General.
  - A Termlet started in the background by the termlet command (no window) listens too; it quits after 10 minutes without requests, and HTTP requests count.
  - After Termlet restarts, there's nothing to reconfigure: calls in progress fail, and it works as usual afterwards.
- By default it only accepts connections from this computer: with "This computer only", it listens on `127.0.0.1` only, so other devices on your network can't connect.
  - For connections from this computer, the Host of a request must be `127.0.0.1`, `localhost` or `[::1]` with this port; requests with an `Origin` header (sent by web pages in a browser) are always refused, so web pages can't call it.
  - Refused requests are noted in the log with the reason (at most once a minute per reason), without the token or the content.
- For an AI tool on a server (for example, Claude Code running on the server in a terminal), use SSH remote forwarding to bring a port on the server back to this computer:
  - Add a remote forwarding rule under [Port forwarding](#port-forwarding): listen on `127.0.0.1:47800` on the server with target `127.0.0.1:47800`; or with OpenSSH: `ssh -R 47800:127.0.0.1:47800 <server>`.
  - The AI tool on the server uses the address `http://127.0.0.1:47800/mcp` and the token as usual. The ports on both ends must match, because Termlet checks the port in the Host.
  - Local paths for `upload_file` and `download_file` are still paths on the computer running Termlet: any program on the server that has the token can read and write files on this computer through it, and run commands on the hosts you've allowed.
  - Other users on the server can reach that port too, with only the token to stop them. Do this only on servers you trust, and close the forwarding when you're done.

### MCP over the LAN

- By default, only AI tools on this computer can connect. To let another computer on your LAN use it (Cursor on another computer, for example), set "Listen on" to "This computer and LAN" under HTTP in the MCP server settings and save: Termlet then listens on the same port on all IPv4 interfaces, and AI tools on this computer keep working.
- Computers on the LAN use this computer's LAN address and a separate LAN token:
  - "LAN address" lists this computer's IPv4 addresses in private ranges; if there are several, pick the one the other computer can reach (those with a default gateway come first; with Tailscale, pick the one starting with 100.).
  - The "LAN token" starts with `termlet_mcp_lan_`. It's generated the first time you choose "This computer and LAN" and save, and can be regenerated on its own, so revoking computers on the LAN doesn't affect AI tools on this computer. The local token is only used on this computer and never sent over the network.
  - The two tokens are told apart by the other side's address: connections from the LAN only accept the LAN token, and those from this computer only the local token.
  - The configuration for that computer: set "For" to "Another computer on the LAN", pick the AI tool and copy it into the user-level configuration of the AI tool on that computer. Claude Desktop can still only use standard input and output.
- Computers on the LAN can only list hosts and run commands: `upload_file` and `download_file` read and write files on this computer, which is no use to the AI over there, so they aren't offered, and calling them anyway returns an explanation. For small text files, use `run_command` (`cat` to read, a heredoc to write).
- Only connections from private ranges are accepted: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16 and 100.64.0.0/10 (Tailscale and the like). Connections from other addresses are closed without reading the request, and the log notes their address.
- For requests from the LAN, the Host must be the address of this computer they connected to, or this computer's name (such as `office-pc` or `office-pc.local`), with the port; requests with an `Origin` header are still always refused.
- The audit log's Source shows "MCP·LAN 192.168.1.23 (client name)", and the approval dialog's title says where the request came from, such as "Cursor (MCP, from 192.168.1.23) wants to run a dangerous command on …".
- Risks:
  - Traffic is unencrypted, with no HTTPS: others on the same network could capture the LAN token and your commands. Use it only on trusted networks, such as at home or in the office, and don't turn it on over public Wi-Fi.
  - Approval dialogs appear on this computer: if nobody's there, dangerous commands are denied after 60 seconds.
  - A computer with the LAN token can run commands on the hosts you've allowed: give it only to computers you trust, and when you no longer need it, switch back to "This computer only" or regenerate the LAN token.
- Firewall: on Windows, the first time Termlet listens on the LAN, Windows Firewall asks whether to allow it on networks; allow it on private networks. If you canceled or nothing appeared, allow Termlet under Firewall & network protection in Windows Security. On other systems with a firewall on, allow this TCP port.
- When you need encryption, use one of these instead of LAN access:
  - Tailscale or a similar tool: traffic goes through an encrypted tunnel, and the other computer connects to this computer's address starting with 100. (again, it can only list hosts and run commands).
  - SSH local forwarding (if this computer runs an SSH server): on the other computer, run `ssh -L 47800:127.0.0.1:47800 <this computer>`, and the AI tool uses `http://127.0.0.1:47800/mcp` with the local token. To Termlet this is a connection from this computer, so all four tools work (file transfers read and write files on this computer); do this only between your own computers.

### Output

- Readable text in a terminal, JSON when captured by a pipe or program (field names and error codes are always English).
- Readable text follows `TERMLET_LANG=en|zh` > `LC_ALL` / `LC_MESSAGES` / `LANG` > the system display language > Chinese (Simplified Chinese and English only; a Traditional Chinese system gets Simplified Chinese).
- When an OpenSSH certificate used to log in (including those for jump hosts) expires in less than an hour, `exec`, `upload` and `download` still finish as usual and also warn you (once it expires, they can't connect any more):
  - the readable output adds a line on stderr saying when it expires and about how many minutes are left, naming the jump host for a jump host's certificate; stdout is unchanged;
  - the JSON gets an `expiringCertificates` field with one entry per certificate: `expiresAt` is the local expiry time (RFC 3339, such as `2026-11-19T20:30:00+08:00`), `minutesLeft` the minutes left, and a jump host's entry also has `jump` (the jump host's name); without such certificates the field is left out;
  - MCP results warn you too;
  - `exec --terminal` uses the connection of the open terminal, which already warned you when it connected, so its result has no such warning.

### Exit codes

| Exit code | Meaning |
|---|---|
| 0 | success |
| 1 | usage error |
| 2 | host not found |
| 3 | CLI access not allowed for the host |
| 4 | the command matched a dangerous command rule and was blocked or not approved |
| 5 | connection or execution failed |
| 6 | credentials or Termlet are locked |
| 7 | the external CLI is off, or Termlet isn't running and may not be started automatically |

- When `exec` runs, the remote command's exit code is passed through.

### Uploads and downloads

- Single files only (archive folders first); an existing file with the same name is overwritten.
- When the server has no SFTP (it refuses the SFTP subsystem), transfers fall back to SCP like the file panel does:
  - the result is the same;
  - the readable output adds a note on stderr, `protocol` in the JSON is `scp` (otherwise `sftp`), and MCP results say so too;
  - SCP can't resume (the CLI doesn't resume anyway; a failed transfer starts over);
  - it needs `scp` and `sh` on the server, and isn't used for hosts set to use a single channel.
- When SCP can't be used either, `sftp_unavailable` is returned with the reason.
- Approvals and auditing work as before.

### Host names and size limits

- `<host>` can be an ID or a name. If several hosts allowed for command-line access share the name, you get an error listing their IDs, so use an ID.
- `--stdin` takes at most 1MB; more is an error and nothing runs.
- stdout and stderr return at most 1MB each; the rest is cut off.

### `exec --terminal`

- Plain `exec` opens a new connection each time, which does not work for servers behind bastion hosts.
- With `--terminal`, the command is typed into the most recently opened terminal of that host in Termlet, where you can see it.
- It inherits the terminal's current directory and environment, and the output and exit code are returned.
- The server needs bash or sh.
- If the terminal is running a program (including full-screen programs and file transfers) you get `terminal_busy`, and `terminal_not_open` if no terminal is open.
- Hosts with "Use a separate credential for AI tools and scripts" cannot use `--terminal` (the terminal runs as you, which would bypass the separate credential); use plain `exec` instead.

### Dangerous commands

- 15 built-in rules (`rm -rf /`, `mkfs`, `shutdown` and more, each of which can be turned off) plus your own regular expressions.
- Multi-line scripts are checked line by line (lines inside multi-line strings and heredocs are checked too and may be flagged by mistake).
- The settings page can test a command against the rules as you type.
- On a match, Termlet asks you in a dialog by default (with the logo of Claude Code or Gemini CLI before the title when one of them is making the request, and a countdown of the seconds left). Rejecting or leaving it for 60 seconds returns exit code 4.
- When you approve a command, its duration covers only running it, and the time spent waiting for you is reported separately: in JSON, `durationMs` is how long it ran and `approvalWaitMs` how long it waited for approval; MCP results and the audit log show them separately too.
- With no Termlet window open, or with the handling set to "Block it", it returns exit code 4 right away.
- This prevents mistakes; it can be bypassed and is not a security boundary.

### Audit

- Every `exec`, `upload` and `download` (including failed, blocked, approved and rejected ones) goes into the local audit log (command, result, exit code, duration, time waiting for approval, calling process chain), up to 100,000 entries.
- Click "View audit log…" under Settings → AI & automation (or "View Audit Log" in the command palette) to view, search, filter, export to CSV / JSON or clear it.

### PATH

- On Windows the command is at `bin\termlet.exe` in the installation folder. "Add to PATH" under Settings → AI & automation adds it to the current user's PATH with one click (effective in newly opened terminals; removed on uninstall, and moved to the new location if you reinstall elsewhere).
- On macOS and Linux, Termlet doesn't change PATH; Settings → AI & automation shows the command to link it yourself (such as `sudo ln -sf /Applications/Termlet.app/Contents/MacOS/bin/termlet /usr/local/bin/termlet`).
- On Linux, the deb and rpm install it as `/usr/bin/termlet`, the tar.gz's `install.sh` links it as `~/.local/bin/termlet`, and the AppImage needs a link you make yourself (see [Linux](#linux)).

### Limits

- The CLI cannot show dialogs, so a host must be connected once in Termlet to confirm its host key (otherwise `hostkey_unknown`).
- Hosts that need a verification code cannot be used from the CLI (`auth_interaction_required`).

## Known limitations

- **Code signing**: the packages are not code-signed. SmartScreen may warn on Windows; the macOS package is only ad-hoc signed and not notarized, so it has to be allowed under System Settings → Privacy & Security the first time.
- **macOS and Linux**:
  - These versions haven't been tested on real hardware yet.
  - Updates are not installed automatically (Termlet announces the new version and opens the download page).
  - Termlet doesn't put the command-line tool on PATH.
  - There is no Windows Hello unlock or WSL.
  - Native crashes (segmentation faults and the like) leave no record (Termlet's own panics are still recorded).
  - On macOS, quitting from the Dock or when shutting down or logging out asks first if connections are open or transfers unfinished, which cancels the shutdown or logout (do it again once Termlet has quit).
  - On macOS, the two shortcut presets, which copy Windows clients, aren't offered.
  - macOS is Apple silicon only, and Linux comes in x64 and arm64 (tar.gz, deb, rpm and AppImage; no Flatpak, Snap or package repository, so the deb and rpm don't upgrade with system updates).
  - The arm64 versions for Windows and Linux are experimental and have never run on real hardware.
- **Portable version**: it isn't entirely traceless. Temporary files for pasted screenshots go to the system's temp folder, the master key is in this computer's Credential Manager if you chose "This computer only", and the Agent Skill is installed in AI tools' folders under your home folder. The command-line tool isn't on PATH, so AI tools need its full path unless you add it to PATH yourself.
- **Untested agents and services**: Pageant and the 1Password and Bitwarden agents have not been tried on real devices; public WebDAV services such as Nutstore and Nextcloud have not been tested either.
- **Serial ports**: cannot send Break.
- **File transfers**: resuming only checks the source's size and modification time, not its content; file management inside WSL does not reconnect automatically; with the remote file name encoding set to GBK or another encoding, following the terminal's folder and command-line transfers still treat names as UTF-8; remote files cannot be dragged straight into File Explorer.
- **Session logs and recordings**: they are never deleted automatically; clean them up yourself.
- **Backups**: local snapshots can only be restored on the same computer; use a full backup to move to another one. Full backups do not include the app lock password, session logs or the interface layout (sidebar width and so on), and hosts that use a key file only keep its path. Automatic snapshots are only taken while Termlet is running.
- **Sync**:
  - when both sides changed the same item, only the newer change is kept (the replaced one can be recovered from the snapshot taken before syncing);
  - choosing to merge on the first sync keeps two copies of a host added separately on both computers;
  - after one computer upgrades Termlet, the others must upgrade to keep syncing;
  - WebDAV supports Basic authentication only;
  - the encrypted sync file in a Gist cannot exceed 8 MB;
  - Gists and S3 services without conditional writes cannot prevent overwrites on the server, so when two computers upload at almost the same moment the later one wins; the earlier one notices on its next sync that its changes are missing and uploads them again, so no data is lost.
- **Key library**: it does not support DSA, Ed448, FIDO (`sk-*`), RSA below 1024 bits, encrypted PKCS#8 or ppk v1.
- **Native crashes**: access violations, stack overflows and other native crashes are recorded too, with a small memory dump saved in the `dumps` folder of the log directory (at most 5, kept on this computer only and never sent automatically; "Show in Folder" under "Crash records…"). Fail-fast crashes (`__fastfail`) bypass every handler and leave no record.
