Termlet（Linux）

运行：解压后直接运行 ./Termlet。

下载页上另有 deb、rpm（由包管理器安装依赖、装进 /opt/termlet，可用包管理器升级与卸载）与 AppImage（一个文件，
加上执行权限即可运行），三者与这个 tar.gz 的程序相同，任选一种。

加进应用菜单（可选）：运行 ./install.sh，不需要 root。它在 ~/.local/share/applications 中写入菜单项
dev.termlet.Termlet.desktop（指向这个目录中的 Termlet），把图标复制到 ~/.local/share/icons/hicolor，并把命令行工具
链接为 ~/.local/bin/termlet。程序留在这个目录中：移动目录后重新运行 ./install.sh；./install.sh --uninstall 删除菜单项、
图标与链接。也可以手动：把 share/ 下的文件复制到 ~/.local/share/，再把菜单项中的 Exec= 改成 Termlet 的完整路径。

要求：x86-64（文件名带 linux-x64 的包）或 ARM64（带 linux-arm64 的包，实验版：没有真机，只在 CI 上编译、测试过），
glibc 2.35 以上（Ubuntu 22.04、Debian 12 及更新的发行版），X11 或 Wayland 的桌面会话；运行时用到
libxcb、libxkbcommon、libxkbcommon-x11 与显卡驱动（Vulkan，Mesa 或显卡厂商的驱动；没有 Vulkan 时尝试 OpenGL ES）。
Debian / Ubuntu 上缺少时：sudo apt install libxcb1 libxkbcommon0 libxkbcommon-x11-0 mesa-vulkan-drivers

数据在 ~/.config/termlet。密码、私钥加密后保存，主密钥放在系统的 Secret Service（GNOME 钥匙串、KWallet 等）中；
没有 Secret Service 时改为设置主密码，每次启动时输入。

更新：Termlet 检查到新版本时提示并打开下载页，不会自动安装；下载新的包，解压后替换这个目录（数据不受影响），
用过 install.sh 的再运行一次。

Linux 版还没有在真机上测试过，遇到问题请到 https://github.com/termlet/termlet/issues 反馈。
下载页：https://termlet.dev/download/ 。Termlet 以 AGPL-3.0-or-later 开源（源代码：https://github.com/termlet/termlet），
许可说明与许可证全文见 license_zh_CN.txt，第三方组件的许可见 licenses/。

----------------------------------------------------------------------------------------------------

Termlet (Linux)

Run: extract the archive and run ./Termlet.

The download page also has a deb and an rpm (the package manager installs the dependencies and puts Termlet in
/opt/termlet, and can upgrade and remove it) and an AppImage (one file; make it executable and run it). They contain
the same program as this tar.gz; pick whichever you like.

Add it to the application menu (optional): run ./install.sh; no root needed. It writes the desktop entry
dev.termlet.Termlet.desktop to ~/.local/share/applications (pointing at Termlet in this directory), copies the icons
to ~/.local/share/icons/hicolor and links the command-line tool as ~/.local/bin/termlet. The program stays in this
directory: run ./install.sh again after moving it; ./install.sh --uninstall removes the entry, icons and link. To do it
by hand, copy the files under share/ to ~/.local/share/ and set Exec= in the desktop entry to the full path of Termlet.

Requirements: x86-64 (packages named linux-x64) or ARM64 (packages named linux-arm64, experimental: no real hardware,
only built and tested in CI), glibc 2.35 or later (Ubuntu 22.04, Debian 12 and newer), an X11 or Wayland desktop session;
at run time it uses libxcb, libxkbcommon, libxkbcommon-x11 and a graphics driver (Vulkan, from Mesa or your GPU
vendor; without Vulkan it tries OpenGL ES).
If something is missing on Debian / Ubuntu: sudo apt install libxcb1 libxkbcommon0 libxkbcommon-x11-0 mesa-vulkan-drivers

Data lives in ~/.config/termlet. Passwords and private keys are stored encrypted; the master key goes into the
system Secret Service (GNOME Keyring, KWallet, ...). Without a Secret Service you set a master password and enter it
at every start.

Updates: when Termlet finds a new version it tells you and opens the download page; it doesn't install updates by
itself. Download the new archive and replace this directory with it (your data is kept); if you used install.sh,
run it again.

The Linux version hasn't been tested on real hardware yet; please report problems at
https://github.com/termlet/termlet/issues .
Download page: https://termlet.dev/en/download/ . Termlet is open source under AGPL-3.0-or-later (source code:
https://github.com/termlet/termlet); see license_en.txt for the license notice and full license text, and licenses/
for third-party licenses.
