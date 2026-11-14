//! 文件页：左边是仓库目录的树，右边是打开的文件（编辑器、图片预览，或者一张「此类文件不在这里显示」的卡片）。
//!
//! 定位是「看和小改」：打开、改几行、保存，新建、改名、删除、上传、下载。不做补全、跳转、调试，
//! 要这些就在 VS Code 里打开。
//!
//! 读写都经 [`Channel`] 的 `fs_*`：本机仓库直接读写磁盘，服务器上的仓库交给 kuaid，路径都相对仓库根。
//!
//! - 树一层层按需列（[`RepoView::list_dir`]），被 .gitignore 忽略的（`target/` 这些）变淡，
//!   改过的、没跟踪的文件标 M、U，里面有改动的目录带一个点。过滤框里有字时不显示树，
//!   按路径列出匹配的文件。
//! - 单击是预览（标签上的名字是斜体，再点别的文件就换掉），双击或者动手改了才留下。
//! - 保存时带着打开时的修改时间和大小：磁盘上的已经不是那样了就不写，在编辑区上面提示。
//! - 仓库有变化（文件监听）时重列展开着的目录，再看一遍打开的文件：没有没保存的修改就直接
//!   重新载入，有就提示冲突。
//! - 画法在 [`super::files_view`]，新建、改名、删除、上传、下载在 [`super::files_ops`]。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use fr_channel::{Channel, FsEntry, FsStat, FsWrite};
use fr_core::i18n::tr;
use fr_core::status::{Kind, Status};
use fr_core::tf;
use futures::FutureExt as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{EditorState, InputEvent, InputState, Position, TabSize};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::{Page, RepoView};

/// 这么大以内的文件在编辑器里打开
pub(super) const TEXT_LIMIT: u64 = 2 << 20;
/// 这么大以内的图片直接显示
pub(super) const IMAGE_LIMIT: u64 = 20 << 20;
/// 过滤时最多列这么多个文件
const MATCH_MAX: usize = 300;
/// 开头这么多字节里有 NUL 就当成二进制
const SNIFF: usize = 8000;

/// 文件的编码。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Encoding {
    Utf8,
    Gb18030,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub(super) fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Gb18030 => "GB18030",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
        }
    }
}

/// 解出来的文字，和写回去要用的那几样。
#[derive(Debug, PartialEq)]
pub(super) struct Decoded {
    /// 换行统一成 `\n`
    pub text: String,
    pub encoding: Encoding,
    /// 开头有 BOM（写回去时照样带上）
    pub bom: bool,
    /// 换行是 CRLF（两种混着的按多的算）
    pub crlf: bool,
    /// 不能编辑的原因：写回去得不到原来的字节
    pub readonly: Option<&'static str>,
}

/// 把文件内容变成能编辑的文字；`None` 是二进制文件。
pub(super) fn decode(bytes: &[u8]) -> Option<Decoded> {
    let (encoding, body, bom) = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (Encoding::Utf8, rest, true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (Encoding::Utf16Le, rest, true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (Encoding::Utf16Be, rest, true)
    } else {
        (Encoding::Utf8, bytes, false)
    };
    let mut readonly = None;
    let (text, encoding) = match encoding {
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let decoder = match encoding {
                Encoding::Utf16Le => encoding_rs::UTF_16LE,
                _ => encoding_rs::UTF_16BE,
            };
            let (text, _) = decoder.decode_without_bom_handling(body);
            // 编码库写不出 UTF-16
            readonly = Some(tr("UTF-16 编码的文件只能查看。"));
            (text.into_owned(), encoding)
        }
        _ => {
            if body[..body.len().min(SNIFF)].contains(&0) {
                return None;
            }
            match std::str::from_utf8(body) {
                Ok(text) => (text.to_string(), Encoding::Utf8),
                Err(_) => {
                    let (text, _, bad) = encoding_rs::GB18030.decode(body);
                    // 解出来再编回去和原来一样才让改：不然没动过的地方也会被写坏
                    let (back, _, lossy) = encoding_rs::GB18030.encode(&text);
                    if bad || lossy || back.as_ref() != body {
                        readonly = Some(tr("无法识别此文件的编码，只能查看。"));
                    }
                    (text.into_owned(), Encoding::Gb18030)
                }
            }
        }
    };
    let crlf_count = text.matches("\r\n").count();
    let crlf = crlf_count > 0 && crlf_count * 2 >= text.matches('\n').count();
    Some(Decoded {
        text: if crlf_count > 0 {
            text.replace("\r\n", "\n")
        } else {
            text
        },
        encoding,
        bom,
        crlf,
        readonly,
    })
}

/// 把编辑器里的文字变回要写进文件的字节。
pub(super) fn encode(text: &str, encoding: Encoding, bom: bool, crlf: bool) -> Vec<u8> {
    let text = match crlf {
        true => std::borrow::Cow::Owned(text.replace('\n', "\r\n")),
        false => std::borrow::Cow::Borrowed(text),
    };
    let mut bytes = Vec::with_capacity(text.len() + 3);
    match encoding {
        Encoding::Gb18030 => bytes.extend_from_slice(&encoding_rs::GB18030.encode(&text).0),
        // UTF-16 的文件是只读的，走不到这里；真走到了按 UTF-8 写
        _ => {
            if bom {
                bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            bytes.extend_from_slice(text.as_bytes());
        }
    }
    bytes
}

/// 按扩展名分：在编辑器里打开、当图片显示，还是不在这里显示。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Class {
    Text,
    Image(ImageFormat),
    /// 不在这里显示的文件，带着它是哪一类（「PDF 文档」「视频」……）
    Opaque(&'static str),
}

pub(super) fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

pub(super) fn classify(path: &str) -> Class {
    match extension(path).as_str() {
        "png" => Class::Image(ImageFormat::Png),
        "jpg" | "jpeg" => Class::Image(ImageFormat::Jpeg),
        "gif" => Class::Image(ImageFormat::Gif),
        "webp" => Class::Image(ImageFormat::Webp),
        "svg" => Class::Image(ImageFormat::Svg),
        "bmp" => Class::Image(ImageFormat::Bmp),
        "ico" => Class::Image(ImageFormat::Ico),
        "pdf" => Class::Opaque(tr("PDF 文档")),
        "doc" | "docx" | "rtf" | "odt" | "wps" => Class::Opaque(tr("Word 文档")),
        "xls" | "xlsx" | "xlsm" | "ods" | "et" => Class::Opaque(tr("Excel 表格")),
        "ppt" | "pptx" | "odp" | "dps" => Class::Opaque(tr("PowerPoint 演示文稿")),
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "flv" | "wmv" | "m4v" | "ts" if is_video(path) => {
            Class::Opaque(tr("视频"))
        }
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" => {
            Class::Opaque(tr("音频"))
        }
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "jar" | "war"
        | "whl" | "deb" | "rpm" | "apk" | "iso" | "dmg" => Class::Opaque(tr("压缩包")),
        "ttf" | "otf" | "woff" | "woff2" | "eot" => Class::Opaque(tr("字体")),
        "tif" | "tiff" | "psd" | "ai" | "heic" | "avif" | "raw" | "sketch" | "fig" => {
            Class::Opaque(tr("图片"))
        }
        "exe" | "dll" | "so" | "dylib" | "bin" | "o" | "a" | "lib" | "obj" | "class" | "pyc"
        | "wasm" | "db" | "sqlite" | "sqlite3" | "pdb" | "rlib" | "rmeta" => {
            Class::Opaque(tr("二进制文件"))
        }
        _ => Class::Text,
    }
}

/// `.ts` 多半是 TypeScript，不是视频；别的视频扩展名没有歧义。
fn is_video(path: &str) -> bool {
    extension(path) != "ts"
}

/// 语法高亮用的语言名（组件库认的），和状态栏里显示的名字。
pub(super) fn language(path: &str) -> (&'static str, &'static str) {
    let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    match name.as_str() {
        "makefile" | "gnumakefile" => return ("make", "Makefile"),
        "dockerfile" => return ("bash", "Dockerfile"),
        "cmakelists.txt" => return ("cmake", "CMake"),
        "cargo.lock" => return ("toml", "TOML"),
        ".gitignore" | ".gitattributes" | ".dockerignore" | ".editorconfig" | ".env" => {
            return ("bash", tr("配置"));
        }
        _ => {}
    }
    match extension(path).as_str() {
        "rs" => ("rust", "Rust"),
        "go" => ("go", "Go"),
        "py" | "pyi" => ("python", "Python"),
        "js" | "mjs" | "cjs" | "jsx" => ("javascript", "JavaScript"),
        "ts" | "mts" | "cts" => ("typescript", "TypeScript"),
        "tsx" => ("tsx", "TSX"),
        "json" | "jsonc" | "json5" | "map" => ("json", "JSON"),
        "toml" => ("toml", "TOML"),
        "yaml" | "yml" => ("yaml", "YAML"),
        "md" | "mdx" | "markdown" => ("markdown", "Markdown"),
        "html" | "htm" | "xml" | "vue" | "xaml" | "plist" => ("html", "HTML"),
        "svelte" => ("svelte", "Svelte"),
        "astro" => ("astro", "Astro"),
        "css" | "scss" | "less" => ("css", "CSS"),
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "conf" | "ini" | "cfg"
        | "properties" => ("bash", "Shell"),
        "c" | "h" => ("c", "C"),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "ino" => ("cpp", "C++"),
        "cs" => ("csharp", "C#"),
        "java" => ("java", "Java"),
        "kt" | "kts" => ("kotlin", "Kotlin"),
        "swift" => ("swift", "Swift"),
        "scala" | "sc" => ("scala", "Scala"),
        "rb" | "gemspec" | "rake" => ("ruby", "Ruby"),
        "php" => ("php", "PHP"),
        "lua" => ("lua", "Lua"),
        "zig" => ("zig", "Zig"),
        "ex" | "exs" => ("elixir", "Elixir"),
        "sql" => ("sql", "SQL"),
        "proto" => ("proto", "Protobuf"),
        "graphql" | "gql" => ("graphql", "GraphQL"),
        "cmake" => ("cmake", "CMake"),
        "mk" => ("make", "Makefile"),
        "diff" | "patch" => ("diff", "Diff"),
        "erb" => ("erb", "ERB"),
        "ejs" => ("ejs", "EJS"),
        _ => ("text", tr("纯文本")),
    }
}

/// 这种文件默认自动换行（文档、纯文本）；代码默认不换。
fn wraps(path: &str) -> bool {
    matches!(language(path).0, "markdown" | "text")
}

/// 看开头几百行猜缩进：用制表符还是空格，空格的话几个一级。
fn indent_of(text: &str) -> TabSize {
    let (mut tabs, mut twos, mut fours) = (0, 0, 0);
    for line in text.lines().take(400) {
        let spaces = line.len() - line.trim_start_matches(' ').len();
        if line.starts_with('\t') {
            tabs += 1;
        } else if spaces == 2 {
            twos += 1;
        } else if spaces == 4 {
            fours += 1;
        }
    }
    TabSize {
        tab_size: if twos > 0 && twos * 2 >= fours { 2 } else { 4 },
        hard_tabs: tabs > twos + fours,
    }
}

/// 在编辑器里打开的文件。
pub(super) struct TextDoc {
    pub editor: Entity<EditorState>,
    /// 和磁盘上一样的那份文字（换行已经统一成 `\n`）
    saved: SharedString,
    /// 有没保存的修改
    pub dirty: bool,
    pub encoding: Encoding,
    pub bom: bool,
    pub crlf: bool,
    pub readonly: Option<&'static str>,
    pub wrap: bool,
    /// 状态栏里显示的语言名
    pub language: &'static str,
    pub saving: bool,
    /// 磁盘上的被别的程序改了，这里又有没保存的修改
    pub conflict: bool,
    save_task: Option<Task<()>>,
    _subscription: Subscription,
}

/// 直接显示的图片。
pub(super) struct ImageDoc {
    pub image: Arc<Image>,
    /// 宽、高（读得出来的话）
    pub size: Option<(u32, u32)>,
}

/// 不在这里显示的文件为什么不显示。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Opaque {
    /// 这一类文件（PDF、表格、视频……）不显示
    Kind(&'static str),
    /// 内容不是文字
    Binary,
    /// 太大
    Large,
}

/// 本机上的一份副本：服务器上的文件拿默认程序打开时先下载到这里。
pub(super) struct LocalCopy {
    pub path: PathBuf,
    /// 下载好时的修改时间，之后变了就是在本机改过
    pub mtime: Option<std::time::SystemTime>,
    pub changed: bool,
}

/// 不在这里显示的文件：一张卡片，给出「用默认程序打开」「下载」。
pub(super) struct OpaqueDoc {
    pub reason: Opaque,
    /// 正在下载（或者传回去）：说明、已经传了多少、一共多少
    pub busy: Option<(SharedString, u64, u64)>,
    pub local: Option<LocalCopy>,
    pub(super) task: Option<Task<()>>,
    pub(super) poll: Option<Task<()>>,
}

pub(super) enum Body {
    Loading,
    Failed(SharedString),
    Text(TextDoc),
    Image(ImageDoc),
    Opaque(OpaqueDoc),
}

/// 一个打开的文件（编辑区上面的一个标签）。
pub(super) struct OpenFile {
    pub id: u64,
    /// 相对仓库根
    pub path: String,
    /// 预览：再点别的文件就把它换掉
    pub preview: bool,
    pub body: Body,
    /// 载入、保存时磁盘上的样子
    pub stat: FsStat,
    /// 磁盘上的文件没了
    pub deleted: bool,
    /// 磁盘上的变了，等着重新载入（要有窗口才能做，画之前处理）
    stale: bool,
    /// 每次载入、保存加一：之前发出去的查询回来时对不上就不算数
    epoch: u64,
    load_task: Option<Task<()>>,
    stat_task: Option<Task<()>>,
}

impl OpenFile {
    pub(super) fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    pub(super) fn text(&self) -> Option<&TextDoc> {
        match &self.body {
            Body::Text(doc) => Some(doc),
            _ => None,
        }
    }

    pub(super) fn text_mut(&mut self) -> Option<&mut TextDoc> {
        match &mut self.body {
            Body::Text(doc) => Some(doc),
            _ => None,
        }
    }

    pub(super) fn opaque_mut(&mut self) -> Option<&mut OpaqueDoc> {
        match &mut self.body {
            Body::Opaque(doc) => Some(doc),
            _ => None,
        }
    }

    pub(super) fn dirty(&self) -> bool {
        self.text().is_some_and(|doc| doc.dirty)
    }
}

/// 树里的一行。过滤时是匹配到的一个文件。
#[derive(Clone)]
pub(super) struct TreeRow {
    pub path: String,
    pub name: SharedString,
    pub depth: usize,
    pub dir: bool,
    pub link: bool,
    /// 展开着的目录
    pub open: bool,
    /// 被 .gitignore 忽略（自己或者上面的目录）
    pub ignored: bool,
    /// 过滤时：所在的目录，写在名字后面
    pub parent: Option<SharedString>,
}

/// 正在进行的一批上传或者一次下载（侧栏最下面的一条）。
pub(super) struct Transfer {
    pub id: u64,
    pub label: SharedString,
    pub sent: u64,
    pub total: u64,
    /// 丢弃就是取消
    pub(super) _task: Task<()>,
}

pub(super) struct Files {
    /// 列过的目录：路径（仓库根是空字符串）→ 里面的东西，排好了序
    dirs: HashMap<String, Vec<FsEntry>>,
    listing: HashMap<String, Task<()>>,
    expanded: HashSet<String>,
    pub selected: Option<String>,
    rows: Arc<Vec<TreeRow>>,
    pub filter: Entity<InputState>,
    needle: String,
    /// 仓库里所有的文件（跟踪的，和没跟踪又没被忽略的），过滤时用；第一次过滤时才读
    all_files: Option<Arc<Vec<String>>>,
    all_task: Option<Task<()>>,
    pub show_ignored: bool,
    /// 被忽略的路径（`git ls-files -o -i --directory` 给的，目录不带末尾的斜杠）
    ignored: HashSet<String>,
    ignored_task: Option<Task<()>>,
    /// 有改动的文件 → 是哪种改动
    marks: HashMap<String, Kind>,
    /// 里面有改动的目录
    dirty_dirs: HashSet<String>,
    pub scroll: UniformListScrollHandle,
    pub focus: FocusHandle,
    /// 要在树里显示出来的路径：它上面的目录都列好以后滚过去
    reveal: Option<String>,
    pub tabs: Vec<OpenFile>,
    pub active: usize,
    next_id: u64,
    pub transfers: Vec<Transfer>,
    /// 根目录列不出来的原因
    pub error: Option<SharedString>,
    /// 根目录列过了
    pub loaded: bool,
    /// 拖着本机的文件停在哪个目录上（仓库根是空字符串）
    pub drop_target: Option<String>,
}

impl Files {
    pub(super) fn new(window: &mut Window, cx: &mut Context<RepoView>) -> (Self, Subscription) {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr("按名称过滤文件…")));
        let subscription = cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.files_filter_changed(cx);
            }
        });
        let files = Self {
            dirs: HashMap::new(),
            listing: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            rows: Arc::default(),
            filter,
            needle: String::new(),
            all_files: None,
            all_task: None,
            show_ignored: true,
            ignored: HashSet::new(),
            ignored_task: None,
            marks: HashMap::new(),
            dirty_dirs: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            reveal: None,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            transfers: Vec::new(),
            error: None,
            loaded: false,
            drop_target: None,
        };
        (files, subscription)
    }

    pub(super) fn rows(&self) -> Arc<Vec<TreeRow>> {
        self.rows.clone()
    }

    pub(super) fn filtering(&self) -> bool {
        !self.needle.is_empty()
    }

    pub(super) fn active_tab(&self) -> Option<&OpenFile> {
        self.tabs.get(self.active)
    }

    pub(super) fn tab_mut(&mut self, id: u64) -> Option<&mut OpenFile> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    pub(super) fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    pub(super) fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// 这个文件是哪种改动（树里行尾的字母、标签上名字的颜色）。
    pub(super) fn mark(&self, path: &str) -> Option<Kind> {
        self.marks.get(path).copied()
    }

    pub(super) fn dir_dirty(&self, path: &str) -> bool {
        self.dirty_dirs.contains(path)
    }

    /// 这个目录里现在列着的名字（没列过为空）。
    pub(super) fn names_in(&self, dir: &str) -> Vec<String> {
        self.dirs
            .get(dir)
            .map(|entries| entries.iter().map(|entry| entry.name.clone()).collect())
            .unwrap_or_default()
    }

    /// 树里这个路径是不是目录（没列到的当成文件）。
    pub(super) fn is_dir(&self, path: &str) -> bool {
        let (parent, name) = split(path);
        self.dirs
            .get(parent)
            .and_then(|entries| entries.iter().find(|entry| entry.name == name))
            .is_some_and(|entry| entry.dir)
    }

    /// 工作区状态变了：重算哪些文件、目录要标出来。
    pub(super) fn set_status(&mut self, status: &Status) {
        self.marks.clear();
        self.dirty_dirs.clear();
        for entry in &status.entries {
            let kind = match (entry.unstaged, entry.staged) {
                (Some(Kind::Conflicted), _) => Kind::Conflicted,
                (Some(Kind::Untracked), _) => Kind::Untracked,
                (_, Some(Kind::Added)) => Kind::Added,
                (Some(kind), _) | (None, Some(kind)) => kind,
                (None, None) => continue,
            };
            self.marks.insert(entry.path.clone(), kind);
            if kind == Kind::Deleted {
                continue;
            }
            let mut dir = entry.path.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                if !self.dirty_dirs.insert(parent.to_string()) {
                    break;
                }
                dir = parent;
            }
        }
    }

    /// 按现在列着的目录、展开的状态（或者过滤框里的字）重排树里的行。
    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        if self.filtering() {
            let needle = self.needle.to_lowercase();
            let files = self.all_files.clone().unwrap_or_default();
            // 名字里有的排前面，只是路径里有的排后面
            let (mut by_name, mut by_path) = (Vec::new(), Vec::new());
            for path in files.iter() {
                let lower = path.to_lowercase();
                if !lower.contains(&needle) {
                    continue;
                }
                let (parent, name) = split(path);
                let row = TreeRow {
                    path: path.clone(),
                    name: name.to_string().into(),
                    depth: 0,
                    dir: false,
                    link: false,
                    open: false,
                    ignored: false,
                    parent: (!parent.is_empty()).then(|| parent.to_string().into()),
                };
                match name.to_lowercase().contains(&needle) {
                    true => by_name.push(row),
                    false => by_path.push(row),
                }
                if by_name.len() >= MATCH_MAX {
                    break;
                }
            }
            rows = by_name;
            rows.extend(by_path);
            rows.truncate(MATCH_MAX);
        } else {
            self.walk("", 0, false, &mut rows);
        }
        self.rows = Arc::new(rows);
        if let Some(path) = &self.reveal
            && let Some(at) = self.rows.iter().position(|row| row.path == *path)
        {
            self.scroll.scroll_to_item(at, ScrollStrategy::Center);
            self.reveal = None;
        }
    }

    fn walk(&self, dir: &str, depth: usize, inherited: bool, rows: &mut Vec<TreeRow>) {
        let Some(entries) = self.dirs.get(dir) else {
            return;
        };
        for entry in entries {
            let path = join(dir, &entry.name);
            let ignored = inherited || self.ignored.contains(&path);
            if ignored && !self.show_ignored {
                continue;
            }
            let open = entry.dir && self.expanded.contains(&path);
            rows.push(TreeRow {
                path: path.clone(),
                name: entry.name.clone().into(),
                depth,
                dir: entry.dir,
                link: entry.link,
                open,
                ignored,
                parent: None,
            });
            if open {
                self.walk(&path, depth + 1, ignored, rows);
            }
        }
    }
}

/// 拆成所在的目录和名字。
pub(super) fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

pub(super) fn join(dir: &str, name: &str) -> String {
    match dir.is_empty() {
        true => name.to_string(),
        false => format!("{dir}/{name}"),
    }
}

/// 目录在前，名字不分大小写地排。
fn sort_entries(entries: &mut [FsEntry]) {
    entries.sort_by_cached_key(|entry| (!entry.dir, entry.name.to_lowercase()));
}

/// 文件大小写成人看的：`812 B`、`3.4 KB`、`12.0 MB`。
pub(super) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

impl RepoView {
    /// 仓库根在磁盘上的完整路径加上相对路径（状态栏、复制路径用）。服务器上的用 `/`，本机用 `\`。
    pub(super) fn full_path(&self, path: &str) -> String {
        let root = self.entry.path.trim_end_matches(['/', '\\']);
        if self.entry.server.is_some() {
            return match path.is_empty() {
                true => root.to_string(),
                false => format!("{root}/{path}"),
            };
        }
        let root = root.replace('/', "\\");
        match path.is_empty() {
            true => root,
            false => format!("{root}\\{}", path.replace('/', "\\")),
        }
    }

    /// 没保存的文件有几个（关标签页、退出前要问）。
    pub fn unsaved_files(&self) -> usize {
        self.files.tabs.iter().filter(|tab| tab.dirty()).count()
    }

    /// 画之前调用：在文件页上、根目录还没列过就列；等着重新载入的文件载入。
    pub(super) fn sync_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Files {
            return;
        }
        if !self.files.loaded
            && self.files.error.is_none()
            && !self.files.listing.contains_key("")
            && self.channel.is_some()
        {
            self.list_dir(String::new(), cx);
            self.load_ignored(cx);
        }
        let stale: Vec<u64> = self
            .files
            .tabs
            .iter_mut()
            .filter_map(|tab| std::mem::take(&mut tab.stale).then_some(tab.id))
            .collect();
        for id in stale {
            self.load_file(id, window, cx);
        }
    }

    /// 切到文件页时：焦点放到打开的文件上，没有就放到树上。
    pub(super) fn focus_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.files.active_tab().and_then(OpenFile::text) {
            Some(doc) => doc.editor.focus_handle(cx).focus(window, cx),
            None => self.files.focus.focus(window, cx),
        }
    }

    /// 键盘焦点是不是在文件页里（树、过滤框或者编辑器）。
    pub(super) fn files_focused(&self, window: &Window, cx: &App) -> bool {
        self.files.focus.is_focused(window)
            || self.files.filter.focus_handle(cx).is_focused(window)
            || self
                .files
                .tabs
                .iter()
                .filter_map(OpenFile::text)
                .any(|doc| doc.editor.focus_handle(cx).is_focused(window))
    }

    /// 列一个目录（相对仓库根，空字符串是根）。
    pub(super) fn list_dir(&mut self, dir: String, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let request = channel.fs_list(&self.entry.path, &dir);
        let key = dir.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                this.files.listing.remove(&dir);
                match result {
                    Ok(mut entries) => {
                        sort_entries(&mut entries);
                        if dir.is_empty() {
                            this.files.loaded = true;
                            this.files.error = None;
                        }
                        this.files.dirs.insert(dir, entries);
                    }
                    Err(err) if dir.is_empty() => {
                        this.files.error =
                            Some(tf!("读取目录失败：{err}", err = format!("{:#}", err)).into());
                    }
                    // 目录没了（被删、被改名）：收起来
                    Err(_) => {
                        this.files.dirs.remove(&dir);
                        this.files.expanded.remove(&dir);
                    }
                }
                this.files.rebuild();
                cx.notify();
            })
            .ok();
        });
        self.files.listing.insert(key, task);
    }

    /// 读一遍被忽略的路径（树里变淡的那些）。
    fn load_ignored(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let args = [
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ];
        let request = channel.git(&self.entry.path, args.map(String::from).to_vec());
        self.files.ignored_task = Some(cx.spawn(async move |this, cx| {
            let Ok(out) = request.await.and_then(|out| out.ok()) else {
                return;
            };
            let ignored: HashSet<String> = String::from_utf8_lossy(&out)
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(|path| path.trim_end_matches('/').to_string())
                .collect();
            this.update(cx, |this, cx| {
                if this.files.ignored != ignored {
                    this.files.ignored = ignored;
                    this.files.rebuild();
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// 读一遍仓库里所有的文件（过滤用）。
    fn load_all_files(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let args = [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ];
        let request = channel.git(&self.entry.path, args.map(String::from).to_vec());
        self.files.all_task = Some(cx.spawn(async move |this, cx| {
            let Ok(out) = request.await.and_then(|out| out.ok()) else {
                return;
            };
            let mut files: Vec<String> = String::from_utf8_lossy(&out)
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_string)
                .collect();
            files.sort_unstable();
            files.dedup();
            this.update(cx, |this, cx| {
                this.files.all_files = Some(Arc::new(files));
                this.files.rebuild();
                cx.notify();
            })
            .ok();
        }));
    }

    fn files_filter_changed(&mut self, cx: &mut Context<Self>) {
        let needle = self.files.filter.read(cx).value().trim().to_string();
        if needle == self.files.needle {
            return;
        }
        self.files.needle = needle;
        if self.files.filtering() && self.files.all_files.is_none() && self.files.all_task.is_none()
        {
            self.load_all_files(cx);
        }
        self.files.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.files.rebuild();
        cx.notify();
    }

    /// 仓库有变化、或者刚做完改动：重列展开着的目录，再看一遍打开的文件。
    pub(super) fn refresh_files(&mut self, cx: &mut Context<Self>) {
        if !self.files.loaded && self.files.tabs.is_empty() {
            return;
        }
        if self.files.loaded {
            // 收起来的目录不留着：下次展开时重新列
            let expanded = self.files.expanded.clone();
            self.files
                .dirs
                .retain(|dir, _| dir.is_empty() || expanded.contains(dir));
            let dirs: Vec<String> = self.files.dirs.keys().cloned().collect();
            for dir in dirs {
                self.list_dir(dir, cx);
            }
            self.load_ignored(cx);
            if self.files.all_files.is_some() {
                self.load_all_files(cx);
            }
        }
        self.check_open_files(cx);
    }

    /// 工作区状态读回来了：树里的标记跟着变。
    pub(super) fn files_status_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(status) = &self.status {
            self.files.set_status(status);
        }
        if self.page == Page::Files || self.files.loaded {
            self.refresh_files(cx);
        }
    }

    /// 看一遍打开的文件在磁盘上变了没有。
    fn check_open_files(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        for tab in &mut self.files.tabs {
            let saving = tab.text().is_some_and(|doc| doc.saving);
            if saving || matches!(tab.body, Body::Loading) {
                continue;
            }
            let (id, epoch) = (tab.id, tab.epoch);
            let request = channel.fs_stat(&repo, &tab.path);
            tab.stat_task = Some(cx.spawn(async move |this, cx| {
                let Ok(stat) = request.await else {
                    return;
                };
                this.update(cx, |this, cx| this.file_stat_checked(id, epoch, stat, cx))
                    .ok();
            }));
        }
    }

    fn file_stat_checked(&mut self, id: u64, epoch: u64, stat: FsStat, cx: &mut Context<Self>) {
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        // 查的这段时间里重新载入过、保存过：这次查到的是之前的样子，不算数
        if tab.epoch != epoch {
            return;
        }
        if !stat.exists {
            if !tab.deleted {
                tab.deleted = true;
                cx.notify();
            }
            return;
        }
        let same = stat.mtime == tab.stat.mtime && stat.size == tab.stat.size;
        if same && !tab.deleted {
            return;
        }
        tab.deleted = false;
        match tab.text_mut() {
            Some(doc) if doc.dirty => doc.conflict = true,
            // 没有没保存的修改：直接换成磁盘上的
            _ => tab.stale = true,
        }
        cx.notify();
    }

    // ---------- 树 ----------

    /// 点树里的一行。目录是展开、收起；文件单击预览，双击留下。
    pub(super) fn click_tree_row(
        &mut self,
        row: &TreeRow,
        clicks: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.files.selected = Some(row.path.clone());
        self.files.focus.focus(window, cx);
        if row.dir {
            self.toggle_dir(row.path.clone(), cx);
        } else {
            self.open_file(row.path.clone(), clicks < 2, window, cx);
        }
        cx.notify();
    }

    pub(super) fn toggle_dir(&mut self, dir: String, cx: &mut Context<Self>) {
        if !self.files.expanded.remove(&dir) {
            self.expand_dir(dir, cx);
            return;
        }
        self.files.rebuild();
        cx.notify();
    }

    pub(super) fn expand_dir(&mut self, dir: String, cx: &mut Context<Self>) {
        self.files.expanded.insert(dir.clone());
        if !self.files.dirs.contains_key(&dir) {
            self.list_dir(dir, cx);
        }
        self.files.rebuild();
        cx.notify();
    }

    /// 全部折叠。
    pub(super) fn collapse_tree(&mut self, cx: &mut Context<Self>) {
        self.files.expanded.clear();
        self.files.rebuild();
        cx.notify();
    }

    pub(super) fn toggle_show_ignored(&mut self, cx: &mut Context<Self>) {
        self.files.show_ignored = !self.files.show_ignored;
        self.files.rebuild();
        cx.notify();
    }

    /// 在树里显示出这个路径：上面的目录都展开，选中它，滚到看得见的地方。
    pub(super) fn reveal_in_tree(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.files.filtering() {
            return;
        }
        let mut dir = String::new();
        let (parents, _) = split(path);
        for part in parents.split('/').filter(|part| !part.is_empty()) {
            dir = join(&dir, part);
            self.files.expanded.insert(dir.clone());
            if !self.files.dirs.contains_key(&dir) && !self.files.listing.contains_key(&dir) {
                self.list_dir(dir.clone(), cx);
            }
        }
        self.files.selected = Some(path.to_string());
        self.files.reveal = Some(path.to_string());
        self.files.rebuild();
        cx.notify();
    }

    /// 树上的按键：上下移动，左右收起、展开，回车打开，F2 改名，Delete 删除。
    pub(super) fn tree_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.files.focus.is_focused(window) {
            return;
        }
        let rows = self.files.rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .files
            .selected
            .as_ref()
            .and_then(|path| rows.iter().position(|row| row.path == *path));
        let key = event.keystroke.key.as_str();
        let move_to = |this: &mut Self, at: usize, cx: &mut Context<Self>| {
            this.files.selected = Some(rows[at].path.clone());
            this.files
                .scroll
                .scroll_to_item(at, ScrollStrategy::Nearest);
            cx.notify();
        };
        const PAGE: usize = 15;
        match (key, current) {
            ("down", _) => {
                let at = current.map_or(0, |at| (at + 1).min(rows.len() - 1));
                move_to(self, at, cx);
            }
            ("up", _) => move_to(self, current.map_or(0, |at| at.saturating_sub(1)), cx),
            ("pagedown", _) => {
                let at = current.map_or(0, |at| (at + PAGE).min(rows.len() - 1));
                move_to(self, at, cx);
            }
            ("pageup", _) => move_to(self, current.map_or(0, |at| at.saturating_sub(PAGE)), cx),
            ("home", _) => move_to(self, 0, cx),
            ("end", _) => move_to(self, rows.len() - 1, cx),
            ("right", Some(at)) if rows[at].dir && !rows[at].open => {
                self.expand_dir(rows[at].path.clone(), cx);
            }
            ("left", Some(at)) if rows[at].dir && rows[at].open => {
                self.toggle_dir(rows[at].path.clone(), cx);
            }
            // 已经收着的目录、文件：跳到上一层
            ("left", Some(at)) => {
                let (parent, _) = split(&rows[at].path);
                if let Some(at) = rows.iter().position(|row| row.path == parent) {
                    move_to(self, at, cx);
                }
            }
            ("enter" | "space", Some(at)) => {
                let row = rows[at].clone();
                let clicks = if key == "enter" { 2 } else { 1 };
                self.click_tree_row(&row, clicks, window, cx);
            }
            ("f2", Some(at)) => self.rename_dialog(rows[at].path.clone(), window, cx),
            ("delete", Some(at)) => self.delete_dialog(rows[at].path.clone(), window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    // ---------- 打开的文件 ----------

    /// 打开一个文件。`preview` 为真是预览：占用「预览」的那个标签（没有就新开一个）。
    pub(super) fn open_file(
        &mut self,
        path: String,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(at) = self.files.tabs.iter().position(|tab| tab.path == path) {
            self.files.active = at;
            if !preview {
                self.files.tabs[at].preview = false;
            }
            cx.notify();
            return;
        }
        let id = self.files.next_id();
        let tab = OpenFile {
            id,
            path,
            preview,
            body: Body::Loading,
            stat: FsStat::default(),
            deleted: false,
            stale: false,
            epoch: 0,
            load_task: None,
            stat_task: None,
        };
        // 预览的那个标签（没改过的）换成这个文件，位置不变
        let reuse = self
            .files
            .tabs
            .iter()
            .position(|tab| tab.preview && !tab.dirty());
        match reuse {
            Some(at) => {
                let old = std::mem::replace(&mut self.files.tabs[at], tab);
                self.forget_tab(old);
                self.files.active = at;
            }
            None => {
                self.files.tabs.push(tab);
                self.files.active = self.files.tabs.len() - 1;
            }
        }
        self.load_file(id, window, cx);
        cx.notify();
    }

    /// 从 Git 页过来：切到文件页，打开这个文件并在树里显示出来。
    pub(super) fn show_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.show_page(Page::Files, window, cx);
        self.open_file(path.clone(), false, window, cx);
        self.reveal_in_tree(&path, cx);
    }

    /// 读文件内容，读回来按种类显示。已经在编辑器里开着的（磁盘上变了）重用那个编辑器。
    pub(super) fn load_file(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(channel), Some(tab)) = (self.channel.clone(), self.files.tab_mut(id)) else {
            return;
        };
        let (repo, path) = (self.entry.path.clone(), tab.path.clone());
        let class = classify(&path);
        let request = match class {
            // 不显示的那几类不用读内容，看一眼大小就行
            Class::Opaque(_) => {
                let stat = channel.fs_stat(&repo, &path);
                async move { Ok((stat.await?, Vec::new())) }.boxed()
            }
            Class::Image(_) => channel.fs_open(&repo, &path, IMAGE_LIMIT),
            Class::Text => channel.fs_open(&repo, &path, TEXT_LIMIT),
        };
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        tab.epoch += 1;
        tab.load_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.file_loaded(id, class, result, window, cx)
            })
            .ok();
        }));
    }

    fn file_loaded(
        &mut self,
        id: u64,
        class: Class,
        result: anyhow::Result<(FsStat, Vec<u8>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        tab.epoch += 1;
        cx.notify();
        let (stat, bytes) = match result {
            Ok((stat, _)) if !stat.exists => {
                tab.body = Body::Failed(tr("文件不存在。").into());
                return;
            }
            Ok(loaded) => loaded,
            Err(err) => {
                tab.body =
                    Body::Failed(tf!("读取文件失败：{err}", err = format!("{:#}", err)).into());
                return;
            }
        };
        tab.stat = stat;
        tab.deleted = false;
        let opaque = |reason| {
            Body::Opaque(OpaqueDoc {
                reason,
                busy: None,
                local: None,
                task: None,
                poll: None,
            })
        };
        let limit = match class {
            Class::Image(_) => IMAGE_LIMIT,
            _ => TEXT_LIMIT,
        };
        let decoded = match class {
            Class::Opaque(kind) => {
                // 卡片已经在了（重新查了一遍大小）：下载好的本机副本留着
                if !matches!(tab.body, Body::Opaque(_)) {
                    tab.body = opaque(Opaque::Kind(kind));
                }
                return;
            }
            _ if stat.size > limit => {
                tab.body = opaque(Opaque::Large);
                return;
            }
            Class::Image(format) => {
                let size = image::ImageReader::new(std::io::Cursor::new(&bytes))
                    .with_guessed_format()
                    .ok()
                    .and_then(|reader| reader.into_dimensions().ok());
                tab.body = Body::Image(ImageDoc {
                    image: Arc::new(Image::from_bytes(format, bytes)),
                    size,
                });
                return;
            }
            Class::Text => match decode(&bytes) {
                Some(decoded) => decoded,
                None => {
                    tab.body = opaque(Opaque::Binary);
                    return;
                }
            },
        };
        let path = tab.path.clone();
        let text: SharedString = decoded.text.into();
        // 已经开着：换内容，光标和滚动的位置尽量留在原处
        if let Some(doc) = tab.text_mut() {
            let editor = doc.editor.clone();
            doc.saved = text.clone();
            doc.dirty = false;
            doc.conflict = false;
            doc.encoding = decoded.encoding;
            doc.bom = decoded.bom;
            doc.crlf = decoded.crlf;
            doc.readonly = decoded.readonly;
            editor.update(cx, |state, cx| {
                let (cursor, scroll): (Position, _) =
                    (state.cursor_position(), state.scroll_offset());
                state.set_value(text, window, cx);
                state.set_cursor_position(cursor, window, cx);
                state.set_scroll_offset(scroll, cx);
            });
            return;
        }
        let (lang, label) = language(&path);
        let wrap = wraps(&path);
        let indent = indent_of(&text);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(lang)
                .line_number(true)
                .searchable(true)
                .soft_wrap(wrap)
                .tab_size(indent)
        });
        editor.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
        let subscription = cx.subscribe(&editor, move |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.file_edited(id, cx);
            }
        });
        let focus = self.page == Page::Files && self.files.active_tab().is_some_and(|t| t.id == id);
        if focus
            && !self.files.filter.focus_handle(cx).is_focused(window)
            && !self.files.focus.is_focused(window)
        {
            editor.focus_handle(cx).focus(window, cx);
        }
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        tab.body = Body::Text(TextDoc {
            editor,
            saved: text,
            dirty: false,
            encoding: decoded.encoding,
            bom: decoded.bom,
            crlf: decoded.crlf,
            readonly: decoded.readonly,
            wrap,
            language: label,
            saving: false,
            conflict: false,
            save_task: None,
            _subscription: subscription,
        });
    }

    /// 编辑器里的内容变了：算一下和磁盘上的还一不一样；预览的标签改过就留下。
    fn file_edited(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        let Some(doc) = tab.text_mut() else {
            return;
        };
        let dirty = {
            let state = doc.editor.read(cx);
            state.text().len() != doc.saved.len() || state.value() != doc.saved
        };
        if dirty != doc.dirty {
            doc.dirty = dirty;
            // 撤销回和磁盘上一样了：冲突的提示也不用留着
            if !dirty {
                doc.conflict = false;
            }
            cx.notify();
        }
        if dirty && tab.preview {
            tab.preview = false;
            cx.notify();
        }
    }

    pub(super) fn select_file_tab(
        &mut self,
        at: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if at >= self.files.tabs.len() {
            return;
        }
        self.files.active = at;
        self.focus_files(window, cx);
        cx.notify();
    }

    /// 关掉一个打开的文件。有没保存的修改先问。
    pub(super) fn close_file_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.files.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        if !tab.dirty() {
            self.drop_file_tab(id, window, cx);
            return;
        }
        let name = tab.name().to_string();
        self.confirm(
            tf!("放弃对 {name} 的修改？", name),
            tr("此文件有未保存的修改，关闭后将丢失。"),
            tr("放弃修改"),
            window,
            cx,
            move |this, window, cx| this.drop_file_tab(id, window, cx),
        );
    }

    /// 关掉显示着的那个文件（Ctrl+W）；一个都没开着时返回假，由调用的地方关仓库标签页。
    pub fn close_active_file(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.page != Page::Files {
            return false;
        }
        match self.files.active_tab().map(|tab| tab.id) {
            Some(id) => {
                self.close_file_tab(id, window, cx);
                true
            }
            None => false,
        }
    }

    pub(super) fn drop_file_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(at) = self.files.tab_index(id) else {
            return;
        };
        let tab = self.files.tabs.remove(at);
        self.forget_tab(tab);
        if at < self.files.active || self.files.active >= self.files.tabs.len() {
            self.files.active = self.files.active.saturating_sub(1);
        }
        if self.page == Page::Files {
            self.focus_files(window, cx);
        }
        cx.notify();
    }

    /// 标签关掉以后的收尾：下载到本机的那份副本删掉。
    fn forget_tab(&mut self, tab: OpenFile) {
        if let Body::Opaque(OpaqueDoc {
            local: Some(copy), ..
        }) = &tab.body
            && let Some(dir) = copy.path.parent()
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// 保存显示着的文件（Ctrl+S、工具栏的「保存」）。
    pub fn save_active_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Files {
            return;
        }
        if let Some(id) = self.files.active_tab().map(|tab| tab.id) {
            self.save_file(id, false, window, cx);
        }
    }

    /// 保存。`overwrite` 为真是不管磁盘上的变没变，直接换掉。
    pub(super) fn save_file(
        &mut self,
        id: u64,
        overwrite: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(channel), repo) = (self.channel.clone(), self.entry.path.clone()) else {
            return;
        };
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        let (path, stat, deleted) = (tab.path.clone(), tab.stat, tab.deleted);
        let Some(doc) = tab.text_mut() else {
            return;
        };
        if doc.saving || doc.readonly.is_some() || (!doc.dirty && !deleted) {
            return;
        }
        let text = doc.editor.read(cx).value();
        let bytes = encode(&text, doc.encoding, doc.bom, doc.crlf);
        let params = FsWrite {
            repo,
            path,
            create: false,
            // 文件被删了又保存：重新写出来
            expect: (!overwrite && !deleted).then(|| stat.into()),
        };
        let request = channel.fs_write(params, bytes);
        doc.saving = true;
        doc.save_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                let Some(tab) = this.files.tab_mut(id) else {
                    return;
                };
                tab.epoch += 1;
                cx.notify();
                let written = match result {
                    Ok(written) => written,
                    Err(err) => {
                        if let Some(doc) = tab.text_mut() {
                            doc.saving = false;
                        }
                        let text = tf!("保存失败：{err}", err = format!("{:#}", err));
                        window.push_notification(Notification::error(text), cx);
                        return;
                    }
                };
                if !written.conflict {
                    tab.stat = written.stat;
                    tab.deleted = false;
                    tab.preview = false;
                }
                let Some(doc) = tab.text_mut() else {
                    return;
                };
                doc.saving = false;
                doc.conflict = written.conflict;
                if !written.conflict {
                    // 保存的这段时间里可能又改了
                    doc.dirty = doc.editor.read(cx).value() != text;
                    doc.saved = text;
                    // 本地修改里多了（或者少了）这个文件
                    this.refresh(cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// 冲突提示里的「使用磁盘上的版本」：丢掉这里的修改，重新载入。
    pub(super) fn reload_file(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(doc) = self.files.tab_mut(id).and_then(OpenFile::text_mut) {
            doc.dirty = false;
            doc.conflict = false;
        }
        self.load_file(id, window, cx);
    }

    /// 冲突提示里的「保留我的修改」：提示收起来，之后保存时直接换掉磁盘上的。
    pub(super) fn keep_mine(&mut self, id: u64, cx: &mut Context<Self>) {
        let (Some(channel), repo) = (self.channel.clone(), self.entry.path.clone()) else {
            return;
        };
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        if let Some(doc) = tab.text_mut() {
            doc.conflict = false;
        }
        // 记下磁盘上现在的样子：再保存时和它比
        let (path, epoch) = (tab.path.clone(), tab.epoch);
        let request = channel.fs_stat(&repo, &path);
        tab.stat_task = Some(cx.spawn(async move |this, cx| {
            let Ok(stat) = request.await else {
                return;
            };
            this.update(cx, |this, _| {
                if let Some(tab) = this.files.tab_mut(id)
                    && tab.epoch == epoch
                    && stat.exists
                {
                    tab.stat = stat;
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// 状态栏里的「自动换行」。
    pub(super) fn toggle_wrap(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.files.tab_mut(id).and_then(OpenFile::text_mut) else {
            return;
        };
        doc.wrap = !doc.wrap;
        let wrap = doc.wrap;
        doc.editor
            .update(cx, |state, cx| state.set_soft_wrap(wrap, window, cx));
        cx.notify();
    }

    /// 改名、删除以后：打开着的文件跟着改路径（`to` 为 None 是删了，关掉）。
    pub(super) fn files_moved(
        &mut self,
        from: &str,
        to: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inside = |path: &str| path == from || path.starts_with(&format!("{from}/"));
        let moved = |path: &str, to: &str| format!("{to}{}", &path[from.len()..]);
        match to {
            Some(to) => {
                for tab in &mut self.files.tabs {
                    if inside(&tab.path) {
                        tab.path = moved(&tab.path, to);
                        tab.epoch += 1;
                    }
                }
                // 展开着的目录也跟着改名
                let expanded: Vec<String> = self.files.expanded.drain().collect();
                self.files.expanded = expanded
                    .into_iter()
                    .map(|dir| if inside(&dir) { moved(&dir, to) } else { dir })
                    .collect();
                if self.files.selected.as_deref().is_some_and(inside) {
                    let selected = self.files.selected.take().unwrap_or_default();
                    self.files.selected = Some(moved(&selected, to));
                }
            }
            None => {
                let gone: Vec<u64> = self
                    .files
                    .tabs
                    .iter()
                    .filter(|tab| inside(&tab.path))
                    .map(|tab| tab.id)
                    .collect();
                for id in gone {
                    self.drop_file_tab(id, window, cx);
                }
                self.files.expanded.retain(|dir| !inside(dir));
                if self.files.selected.as_deref().is_some_and(inside) {
                    self.files.selected = None;
                }
            }
        }
        // 列着的目录按新的名字重新列
        self.files.dirs.retain(|dir, _| !inside(dir));
        self.refresh(cx);
        self.refresh_files(cx);
    }

    pub(super) fn files_channel(&self) -> Option<(Channel, String)> {
        Some((self.channel.clone()?, self.entry.path.clone()))
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：界面库也有一个叫 test 的宏，会盖掉标准的 #[test]
    use super::{
        Class, Encoding, classify, decode, encode, extension, human_size, indent_of, language,
    };
    use fr_core::i18n::tr;
    use gpui_kit::ImageFormat;

    #[test]
    fn 按扩展名分类() {
        assert_eq!(classify("a/b/icon.PNG"), Class::Image(ImageFormat::Png));
        assert_eq!(classify("说明.pdf"), Class::Opaque(tr("PDF 文档")));
        assert_eq!(classify("报表.xlsx"), Class::Opaque(tr("Excel 表格")));
        assert_eq!(classify("demo.mp4"), Class::Opaque(tr("视频")));
        // .ts 是 TypeScript，不是视频
        assert_eq!(classify("src/index.ts"), Class::Text);
        // 没有扩展名、隐藏文件：当文字
        assert_eq!(classify("Makefile"), Class::Text);
        assert_eq!(classify(".gitignore"), Class::Text);
        assert_eq!(extension(".gitignore"), "");
        assert_eq!(language("crates/kuaid/src/conn.rs").1, "Rust");
        assert_eq!(language("Makefile").0, "make");
    }

    #[test]
    fn 解码再编码得到原来的字节() {
        let cases: [&[u8]; 6] = [
            b"plain\nlf\n",
            b"crlf\r\nfile\r\nlast",
            // UTF-8 带 BOM
            b"\xEF\xBB\xBFbom\r\n",
            // GBK：“中文”
            b"\xD6\xD0\xCE\xC4\r\nabc\r\n",
            "中文 UTF-8\n".as_bytes(),
            b"",
        ];
        for bytes in cases {
            let decoded = decode(bytes).unwrap();
            assert_eq!(decoded.readonly, None, "{bytes:?}");
            assert!(!decoded.text.contains('\r'));
            let back = encode(&decoded.text, decoded.encoding, decoded.bom, decoded.crlf);
            assert_eq!(back, bytes);
        }
        let gbk = decode(b"\xD6\xD0\xCE\xC4").unwrap();
        assert_eq!(
            (gbk.text.as_str(), gbk.encoding),
            ("中文", Encoding::Gb18030)
        );
        // 有 NUL 的是二进制
        assert!(decode(b"MZ\0\0\x03").is_none());
        // UTF-16 能看不能改
        let utf16 = decode(b"\xFF\xFEa\0b\0").unwrap();
        assert_eq!(utf16.text, "ab");
        assert!(utf16.readonly.is_some());
        // 编不回去的字节：只读
        assert!(decode(b"abc\xFF\xFE\xFDxyz").unwrap().readonly.is_some());
        // 两种换行混着：按多的算
        assert!(!decode(b"a\nb\nc\r\n").unwrap().crlf);
        assert!(decode(b"a\r\nb\r\nc\n").unwrap().crlf);
    }

    #[test]
    fn 猜缩进() {
        let two = indent_of("a:\n  b:\n    c: 1\n  d: 2\n");
        assert_eq!((two.tab_size, two.hard_tabs), (2, false));
        let four = indent_of("fn a() {\n    let x = 1;\n    if x {\n        y();\n    }\n}\n");
        assert_eq!((four.tab_size, four.hard_tabs), (4, false));
        assert!(indent_of("a {\n\tb;\n\tc;\n}\n").hard_tabs);
    }

    #[test]
    fn 大小写成人看的() {
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(3482), "3.4 KB");
        assert_eq!(human_size(12 << 20), "12.0 MB");
    }
}
