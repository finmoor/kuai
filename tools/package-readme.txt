fork-remote（暂名）
==================

Windows 上的 Git 客户端，界面和操作习惯接近 Fork，能直接打开服务器上的仓库。

怎么用
------
1. 双击 fork-remote.exe。不用安装，整个文件夹放哪都行（agents 文件夹要和 exe 在一起）。
2. 「仓库」标签页里：
   - 「添加本机仓库」：填仓库路径。也可以在命令行里 fork-remote.exe <仓库路径> 直接打开。
   - 「添加服务器」：填 ssh 目标（~/.ssh/config 里的主机别名，或者 user@host）。
     第一次连接会把 agents 里的程序上传到服务器的 ~/.fork-remote/ 下，不要 root、不开端口。
     然后「浏览」服务器上的目录，打开仓库。
3. 仓库页：左边是本地修改、所有提交、分支、远程、标签、贮藏；
   本地修改里可以按文件、按块、按行暂存和丢弃，写说明后提交；
   工具栏上有获取、拉取、推送、贮藏、新建分支。

需要
----
- git 在 PATH 里（装了 Git for Windows 就有）。
- 连服务器用的是 Windows 自带的 OpenSSH（C:\Windows\System32\OpenSSH\ssh.exe），
  沿用 ~/.ssh/config 和密钥；要密码或密钥口令时程序会弹窗问。
- 服务器是 Linux，装了 git。

其他
----
- 设置存在 %APPDATA%\fork-remote\settings.json，不存密码。
- 卸载：删掉这个文件夹和上面的设置文件；服务器上删掉 ~/.fork-remote。
- 看日志：在命令行里先设环境变量 FR_LOG=1，再把标准错误重定向到文件，比如
    set FR_LOG=1 && fork-remote.exe 2> log.txt
