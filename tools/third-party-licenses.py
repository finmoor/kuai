"""生成发布包里的第三方许可证清单：Kuai 和 kuaid 编进去的每个库的名字、版本、许可证，
和这些库自带的许可证全文。MIT、Apache-2.0 这类许可证要求随程序附上它们的声明。

用法：python tools/third-party-licenses.py <输出文件> [windows|linux]
（tools/package.ps1 打 Windows 版时调用，默认 windows；tools/package-linux.sh 打 Linux 版时带 linux）

只算编进程序的依赖：这个平台的客户端 fr-app，和包里带的 kuaid（Linux 版、Windows 版）的
普通依赖（连带过程宏），不算只在编译时跑的构建脚本依赖和测试用的。全文一样的许可证只印一次，
后面列出用它的库。另外加上不是 cargo 依赖、原样随包带的程序文件（VENDORED；Linux 版的清单里还有
AppImage 开头的运行时和它静态链接的库，见 vendor/appimage-runtime/README.md）。
Windows 版的清单用 CRLF，Linux 版的用 LF。
"""
import collections
import hashlib
import json
import pathlib
import subprocess
import sys

# 客户端按平台；两个平台的包里带的是同一批 kuaid（见 tools/package-linux.sh）
CLIENT = {'windows': 'x86_64-pc-windows-msvc', 'linux': 'x86_64-unknown-linux-gnu'}
KUAID = ['x86_64-unknown-linux-musl', 'x86_64-pc-windows-msvc']
LICENSE_PREFIXES = ('license', 'licence', 'copying', 'notice', 'unlicense', 'copyright')
REPO = pathlib.Path(__file__).resolve().parent.parent
# 原样随包带的程序文件：许可证全文在 manifest_path 所在的目录里。换版本时和 vendor/conpty/README.md、
# vendor/appimage-runtime/README.md 一起改。platforms：只在这几个平台的包里（不写是都有）
RUNTIME = REPO / 'vendor' / 'appimage-runtime'
VENDORED = [
    {
        'name': 'Microsoft.Windows.Console.ConPTY (conpty.dll, OpenConsole.exe)',
        'version': '1.25.260930003',
        'license': 'MIT',
        'manifest_path': str(REPO / 'vendor' / 'conpty' / 'README.md'),
    },
    {
        'name': 'AppImage type2-runtime (runtime of the .AppImage)',
        'version': '20251108',
        'license': 'MIT',
        'manifest_path': str(RUNTIME / 'README.md'),
        'platforms': ['linux'],
    },
    {
        'name': 'libfuse (in the AppImage runtime)',
        'version': '3.15.0',
        'license': 'LGPL-2.1',
        'manifest_path': str(RUNTIME / 'libfuse' / 'LICENSE'),
        'platforms': ['linux'],
    },
    {
        'name': 'squashfuse (in the AppImage runtime)',
        'version': '0.5.2',
        'license': 'BSD-2-Clause',
        'manifest_path': str(RUNTIME / 'squashfuse' / 'LICENSE'),
        'platforms': ['linux'],
    },
    {
        'name': 'musl libc (in the AppImage runtime)',
        'version': 'Alpine 3.21',
        'license': 'MIT',
        'manifest_path': str(RUNTIME / 'musl' / 'COPYRIGHT'),
        'platforms': ['linux'],
    },
    {
        'name': 'zstd (in the AppImage runtime)',
        'version': 'Alpine 3.21',
        'license': 'BSD-3-Clause',
        'manifest_path': str(RUNTIME / 'zstd' / 'LICENSE'),
        'platforms': ['linux'],
    },
    {
        'name': 'zlib (in the AppImage runtime)',
        'version': 'Alpine 3.21',
        'license': 'Zlib',
        'manifest_path': str(RUNTIME / 'zlib' / 'LICENSE'),
        'platforms': ['linux'],
    },
]


def metadata(target):
    out = subprocess.run(
        ['cargo', 'metadata', '--format-version', '1', '--filter-platform', target],
        capture_output=True, check=True)
    return json.loads(out.stdout)


def runtime_packages(meta, root_name):
    """从 root_name 出发，沿普通依赖走到的所有包（不含工作区自己的）。"""
    packages = {p['id']: p for p in meta['packages']}
    nodes = {n['id']: n for n in meta['resolve']['nodes']}
    members = set(meta['workspace_members'])
    root = next(pid for pid in members if packages[pid]['name'] == root_name)
    seen, stack = set(), [root]
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]['deps']:
            if any(kind['kind'] is None for kind in dep['dep_kinds']):
                stack.append(dep['pkg'])
    return [packages[pid] for pid in seen if pid not in members]


def license_files(package):
    folder = pathlib.Path(package['manifest_path']).parent
    files = [f for f in sorted(folder.iterdir())
             if f.is_file() and f.name.lower().startswith(LICENSE_PREFIXES)]
    if package.get('license_file'):
        extra = folder / package['license_file']
        if extra.is_file() and extra not in files:
            files.append(extra)
    return files


def main():
    out = pathlib.Path(sys.argv[1])
    platform = sys.argv[2] if len(sys.argv) > 2 else 'windows'
    if platform not in CLIENT:
        sys.exit(f'不认识的平台：{platform}（windows 或 linux）')
    roots = [('fr-app', CLIENT[platform])] + [('kuaid', target) for target in KUAID]
    packages = {}
    for root, target in roots:
        for package in runtime_packages(metadata(target), root):
            packages[(package['name'], package['version'])] = package
    for package in VENDORED:
        if platform in package.get('platforms', CLIENT):
            packages[(package['name'], package['version'])] = package
    rows = sorted(packages.values(), key=lambda p: (p['name'].lower(), p['version']))

    texts = collections.OrderedDict()  # 全文的哈希 -> (全文, [用它的库])
    missing = []
    for package in rows:
        found = license_files(package)
        if not found:
            missing.append(package)
        for path in found:
            text = path.read_text(encoding='utf-8', errors='replace').strip()
            key = hashlib.sha256(text.encode('utf-8')).hexdigest()
            texts.setdefault(key, (text, []))[1].append(f"{package['name']} {package['version']}")

    lines = [
        'Kuai 用到的第三方库和它们的许可证',
        'Third-party libraries used by Kuai and their licenses',
        '',
        '由 tools/third-party-licenses.py 生成。Kuai 自己按 GPL-3.0 或更高版本发布，见 LICENSE。',
        'Generated by tools/third-party-licenses.py. Kuai itself is licensed under GPL-3.0-or-later, see LICENSE.',
        '',
        f'{len(rows)} 个库 / libraries:',
        '',
    ]
    width = max(len(f"{p['name']} {p['version']}") for p in rows)
    for package in rows:
        name = f"{package['name']} {package['version']}"
        lines.append(f"  {name.ljust(width)}  {package.get('license') or '?'}")
    if missing:
        lines += ['', '这几个库的源码包里没带许可证文件，许可证见上表 / no license file shipped:']
        lines += [f"  {p['name']} {p['version']}  {p.get('license') or '?'}  {p.get('repository') or ''}"
                  for p in missing]
    for text, users in texts.values():
        lines += ['', '=' * 78, '用到这份许可证的库 / used by: ' + ', '.join(users), '=' * 78, '', text]
    newline = '\r\n' if platform == 'windows' else '\n'
    out.write_text('\n'.join(lines) + '\n', encoding='utf-8', newline=newline)
    print(f'{out}: {len(rows)} libraries, {len(texts)} distinct license texts, '
          f'{len(missing)} without a license file')


if __name__ == '__main__':
    main()
