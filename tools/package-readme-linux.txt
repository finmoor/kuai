Kuai（筷子）Linux 版
====================

远程开发工具：本机和远程主机上的仓库，在同一个界面里管理 Git、终端和文件。
Git 部分的界面和操作方式接近 Fork。功能、用法和快捷键与 Windows 版相同，介绍见
https://github.com/finmoor/kuai 。

安装和运行
----------
这是解压版。Ubuntu、Debian 上也可以用 .deb 安装包（kuai_<版本>_amd64.deb，装在 /opt/kuai），
其他发行版也可以用 AppImage（kuai-<版本>-linux-x64.AppImage，加上可执行权限后直接运行），
均在 https://github.com/finmoor/kuai/releases 下载。

1. 解压后运行 Kuai/kuai。文件夹可放在任意位置（kuaid 文件夹须与 kuai 位于同一目录）。
   也可以在命令行中运行 kuai <仓库路径> 直接打开仓库。
2. 首次运行时会在应用菜单中添加 Kuai（~/.local/share/applications/kuai.desktop），之后可从应用菜单打开。
   移动文件夹后，运行一次新位置的 kuai，菜单项即指向新位置。

运行要求
--------
- x86_64，glibc 2.35 及以上（Ubuntu 22.04、Debian 12、Fedora 36 及更新的版本），X11 或 Wayland 桌面。
- 显卡驱动支持 Vulkan。多数发行版自带的 Mesa 即可，Ubuntu、Debian 上的软件包为 mesa-vulkan-drivers。
- PATH 中有 git 2.31 或更新的版本和 OpenSSH 客户端（ssh），沿用 ~/.ssh/config 和密钥。
- 保存密码和私钥口令需要 Secret Service（GNOME 钥匙圈、KWallet 等）。没有时不保存，连接时弹窗输入。
- 远程主机为 Linux 或开启了 OpenSSH 服务的 Windows，并已安装 git。

与 Windows 版的不同
-------------------
- 托盘图标需要桌面支持 StatusNotifierItem：KDE 自带；Ubuntu 的 GNOME 自带 AppIndicator 扩展；
  其他 GNOME（如 Fedora）需安装「AppIndicator and KStatusNotifierItem Support」扩展。
  桌面没有托盘时，设置中不显示托盘的选项。
- Wayland 桌面上窗口无法隐藏到托盘：「关闭窗口时最小化到托盘」改为最小化窗口，
  没有「最小化到托盘」的选项。X11 桌面上与 Windows 版相同。
- 将主机上的目录映射到本机需要 fuse3（sudo apt install fuse3，Fedora 为 sudo dnf install fuse3），
  只能映射为文件夹（空文件夹或尚不存在的文件夹），没有盘符。
- 密码和私钥口令保存在系统钥匙圈中，名称为「Kuai: <主机地址>」，移除主机时一并删除。
  只有保存时可能弹出系统的对话框（新建或解锁钥匙圈）；钥匙圈锁着时，连接时改为弹窗输入。
- 本机终端默认使用登录 shell（$SHELL），可在设置中改为 bash、zsh、fish 等。
- 「在外部终端中打开」依次查找 x-terminal-emulator、gnome-terminal、ptyxis、kgx、konsole、
  xfce4-terminal、alacritty、kitty、foot、xterm，使用找到的第一个。

文件位置
--------
- 设置：~/.config/kuai/settings.json，不包含密码。
- 仓库列表和工作区：~/.config/kuai/workspaces.json，由本机的 kuaid 读写。
- 本机的 kuaid 和终端：~/.kuai/。查看常驻进程和终端：~/.kuai/*/kuaid --status；
  结束终端并退出常驻进程：~/.kuai/*/kuaid --stop。
- 开机自动启动：~/.config/autostart/kuai.desktop。
- 更新时下载的文件：~/.cache/kuai/update/。

更新
----
有新版本时，标题栏齿轮左侧显示「新版本 x.y.z」，点击可查看更新内容，点击「更新」开始下载，
下载完成后点击「重启以完成更新」，在原文件夹中替换文件。远程主机上和本机 kuaid 中的终端不受影响。
文件夹没有写入权限时无法自动更新，请前往 https://github.com/finmoor/kuai/releases 手动下载。
.deb 安装的会弹出系统的管理员认证，由 apt 安装新的 .deb；AppImage 会替换原来的 .AppImage 文件。

卸载
----
1. 在设置中关闭「开机时自动启动」，移除保存了密码的主机。
2. 删除此文件夹，以及 ~/.local/share/applications/kuai.desktop、
   ~/.local/share/icons/hicolor/256x256/apps/kuai.png、~/.config/kuai、~/.cache/kuai。
3. 不再需要本机终端时，先运行 ~/.kuai/*/kuaid --stop，然后删除 ~/.kuai。
4. 远程主机上：先运行 ~/.kuai/*/kuaid --stop，然后删除 ~/.kuai。

其他
----
- kuaid/x86_64-pc-windows-msvc 中的 conpty.dll、OpenConsole.exe 是微软 Windows Terminal 项目发布的
  新版 ConPTY（MIT 许可），连接 Windows 主机时随 kuaid.exe 一起上传。
- 查看日志：FR_LOG=1 ./kuai 2> log.txt
