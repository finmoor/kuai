# 界面测试脚本

在不打扰正在用电脑的人的前提下驱动客户端窗口做界面测试：截图不用切到前台，鼠标和键盘输入直接投递给测试窗口，不移动真实鼠标、不抢焦点。

| 脚本 | 作用 |
|---|---|
| `make-ui-repo.sh <目录>` | 建测试仓库：有改动、删除、未跟踪、已暂存的文件，分支、标签、贮藏，远程是同目录下的裸仓库 |
| `make-ops-repo.sh <目录>` | 建测合并、变基、挑拣、冲突用的仓库：两条分支改了同一行，合并必然冲突 |
| `capture-window.ps1 -ProcessId <pid> -Out <png>` | 用 PrintWindow 截窗口，窗口被挡住也能截 |
| `post-input.ps1 -ProcessId <pid> -Action <动作> ...` | 投递输入：`click`、`dblclick`、`move`（`-X -Y`），`drag`（再加 `-X2 -Y2`），`wheel`（滚轮，`-Text` 给格数，负数往下滚），`rclick`（右键），`type`（`-Text`，中文也行），`key`（`-Text` 给按键名：enter、esc、f1 到 f12、up、down、left、right、tab、backspace、insert、delete、space、pageup、pagedown、home、end，还有 comma、backtick、equal、minus 和单个字母、数字），`activate`（模拟切走再切回窗口，触发自动刷新），`mclick`（中键），`xclick`（鼠标侧键，`-Text back` 后退、`forward` 前进），`syscommand`（`-Text minimize`、`restore`、`maximize`、`close`），`hittest`（问标题栏上这一点是能点的客户区还是拖窗口的地方）。`click`、`key` 加 `-Mods ctrl`、`shift`、`alt`（可以用加号连起来，如 `ctrl+shift`）就是带修饰键的 |
| `crop.ps1` | 从截图里裁一块放大看 |
| `fake-sshd.py <主目录> <kuaid 程序> <端口> [选项]` | 测试用的 SSH 服务：只听 127.0.0.1，把客户端接到 `<kuaid 程序> --stdio` 上，客户端看到的主机就是 <主目录>。不带选项时登录一律放行；`--password-env` 要密码（加 `--kbd` 改用 keyboard-interactive），`--pubkey` 只认一把密钥，`--forward` 当跳板机。要装 paramiko |
| `throttle-proxy.py <听的端口> <目标端口> <每秒字节数> [开头不发的秒数]` | 测试用的限速转发：接在 fake-sshd 前面，主机到客户端的方向限速，模拟慢线路；第四个参数让每条连接开头几秒不发，模拟握手很慢 |
| `wizard.ps1 -ProcessId <pid> ...` | 驱动安装、卸载程序的向导：`-List` 列控件和窗口句柄，`-Invoke` 点按钮，`-Toggle` 勾选框，`-Bottom` 放到最底下 |
| `make-test-setup.ps1 -PackageDir <Kuai 文件夹> -OutDir <目录> [-Name a] [-Drop <正则>]` | 编一个测试用的安装程序：单独的 AppId 和开始菜单名，卸载时不删真的更新目录 |
| `run-from-explorer.ps1 -File <程序或 .cmd> [-Arguments ...]` | 让资源管理器来启动，和用户双击一样：不继承当前会话的环境变量 |

坐标都按 `capture-window.ps1` 截出来的图量：窗口矩形左上角为原点的物理像素。

## 一次测试的流程（PowerShell）

```powershell
bash tools/ui-test/make-ui-repo.sh $env:TEMP/fr-ui      # 建测试仓库
# 写一个只打开 $env:TEMP/fr-ui/ui-repo 的设置文件，格式见 %APPDATA%\kuai\settings.json
$env:FR_SETTINGS = '<设置文件>'; $env:FR_TEST_VIEW = 'changes'; $env:FR_LOG = '1'
$p = Start-Process target\release\fr-app.exe -ArgumentList '--relaunched' -PassThru -RedirectStandardError ui-err.log
tools/ui-test/capture-window.ps1 -ProcessId $p.Id -Out shot.png   # 看图，量坐标
tools/ui-test/post-input.ps1 -ProcessId $p.Id -Action click -X 430 -Y 256
git -C $env:TEMP/fr-ui/ui-repo diff --cached                         # 用 git 核对结果
```

测试时不碰自己在用的那一套：`FR_SETTINGS` 换设置文件，`FR_KUAID_HOME` 换本机 kuaid 的目录（没设时用设置文件旁边的 `kuaid-home`），`FR_AUTOSTART_KEY` 换开机自启写的注册表键，`FR_SECRETS_PREFIX` 换凭据的前缀，`FR_UPDATE_API`、`FR_UPDATE_DIR` 换检查更新问的地址和下载目录。`FR_LOG=1` 把日志写到标准错误。
