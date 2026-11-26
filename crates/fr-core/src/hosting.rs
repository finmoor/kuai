//! 托管平台的网页：从远程地址推出仓库的网页地址，拼出提交、分支、创建合并请求的链接。
//!
//! 只拼地址，不联网、不登录。github.com 这几个公开的站点直接认得是哪种平台；自己搭的（GitLab、
//! Gitea……）地址上看不出来，由界面问一次、按主机记住。

use serde::{Deserialize, Serialize};

use crate::i18n::tr;

/// 哪种托管平台：各家网页的路径写法不一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Flavor {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
    #[serde(rename = "gitee")]
    Gitee,
    /// Gitea、Forgejo（Codeberg）
    #[serde(rename = "gitea")]
    Gitea,
    #[serde(rename = "bitbucket")]
    Bitbucket,
    /// 认不出来：只能打开仓库首页
    #[serde(rename = "other")]
    Other,
}

impl Flavor {
    pub const ALL: [Flavor; 6] = [
        Flavor::GitHub,
        Flavor::GitLab,
        Flavor::Gitee,
        Flavor::Gitea,
        Flavor::Bitbucket,
        Flavor::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Flavor::GitHub => "GitHub",
            Flavor::GitLab => "GitLab",
            Flavor::Gitee => "Gitee",
            Flavor::Gitea => "Gitea / Forgejo",
            Flavor::Bitbucket => "Bitbucket",
            Flavor::Other => tr("其他"),
        }
    }

    /// 「合并请求」在这个平台上的叫法：GitLab 叫合并请求，其余叫拉取请求。
    pub fn request(self) -> &'static str {
        match self {
            Flavor::GitLab | Flavor::Other => tr("合并请求"),
            _ => tr("拉取请求"),
        }
    }

    /// 公开的站点按主机名直接认。
    pub fn known(host: &str) -> Option<Flavor> {
        let host = host.to_ascii_lowercase();
        let host = host.split(':').next().unwrap_or_default();
        match host {
            "github.com" => Some(Flavor::GitHub),
            "gitlab.com" => Some(Flavor::GitLab),
            "gitee.com" => Some(Flavor::Gitee),
            "bitbucket.org" => Some(Flavor::Bitbucket),
            "codeberg.org" | "gitea.com" => Some(Flavor::Gitea),
            _ => None,
        }
    }
}

/// 一个远程对应的网页。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Web {
    /// 主机（带端口的 https 地址连端口一起），按它记是哪种平台
    pub host: String,
    /// 所有者（路径里仓库名前面那一段，可以有几层）
    pub owner: String,
    /// 仓库首页，末尾不带 `/`
    pub base: String,
}

/// 从远程地址推出网页。认三种写法：`git@主机:组/仓库.git`、`ssh://git@主机:端口/组/仓库.git`、
/// `https://主机/组/仓库.git`。ssh 的端口不是网页的端口，丢掉；本机路径、`file://` 没有网页。
pub fn web(url: &str) -> Option<Web> {
    let url = url.trim();
    let (scheme, host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        let (authority, path) = rest.split_once('/')?;
        // 去掉地址里的账号、密码
        let host = authority.rsplit('@').next()?;
        match scheme.as_str() {
            "http" | "https" => (scheme, host.to_string(), path),
            "ssh" | "git" | "git+ssh" | "ssh+git" => {
                let host = host.split(':').next()?;
                ("https".to_string(), host.to_string(), path)
            }
            _ => return None,
        }
    } else {
        // `用户@主机:路径`；Windows 的 `C:\repo`、不带冒号的本机路径不算
        let (left, path) = url.split_once(':')?;
        if left.len() < 2 || left.contains(['/', '\\']) || path.starts_with(['/', '\\']) {
            return None;
        }
        let host = left.rsplit('@').next()?;
        ("https".to_string(), host.to_string(), path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    if host.is_empty() || !path.contains('/') {
        return None;
    }
    let owner = path.rsplit_once('/').map(|(owner, _)| owner.to_string())?;
    Some(Web {
        host: host.to_ascii_lowercase(),
        owner,
        base: format!("{scheme}://{host}/{}", encode(path)),
    })
}

/// 地址里的一段：分支名这类可以有 `/`，原样留着；空格、`#`、`?`、`%` 和非 ASCII 字符转义。
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 查询参数里的值：`/` 也转义。
fn encode_value(text: &str) -> String {
    encode(text).replace('/', "%2F")
}

impl Web {
    /// 网页上看一个提交；认不出平台时没有。
    pub fn commit(&self, flavor: Flavor, id: &str) -> Option<String> {
        let base = &self.base;
        Some(match flavor {
            Flavor::GitHub | Flavor::Gitee | Flavor::Gitea => format!("{base}/commit/{id}"),
            Flavor::GitLab => format!("{base}/-/commit/{id}"),
            Flavor::Bitbucket => format!("{base}/commits/{id}"),
            Flavor::Other => return None,
        })
    }

    /// 网页上看一个分支。
    pub fn branch(&self, flavor: Flavor, branch: &str) -> Option<String> {
        let (base, branch) = (&self.base, encode(branch));
        Some(match flavor {
            Flavor::GitHub | Flavor::Gitee => format!("{base}/tree/{branch}"),
            Flavor::GitLab => format!("{base}/-/tree/{branch}"),
            Flavor::Gitea => format!("{base}/src/branch/{branch}"),
            Flavor::Bitbucket => format!("{base}/branch/{branch}"),
            Flavor::Other => return None,
        })
    }

    /// 从 `branch` 创建合并请求的页面。`target` 是要合并到的分支（远程的默认分支），Gitee、Gitea
    /// 的地址里必须有它，不知道时没有链接。
    pub fn new_request(
        &self,
        flavor: Flavor,
        branch: &str,
        target: Option<&str>,
    ) -> Option<String> {
        let base = &self.base;
        Some(match flavor {
            Flavor::GitHub => match target {
                Some(target) => format!(
                    "{base}/compare/{}...{}?expand=1",
                    encode(target),
                    encode(branch)
                ),
                None => format!("{base}/compare/{}?expand=1", encode(branch)),
            },
            Flavor::GitLab => {
                let mut url = format!(
                    "{base}/-/merge_requests/new?merge_request%5Bsource_branch%5D={}",
                    encode_value(branch)
                );
                if let Some(target) = target {
                    url.push_str(&format!(
                        "&merge_request%5Btarget_branch%5D={}",
                        encode_value(target)
                    ));
                }
                url
            }
            Flavor::Gitee => {
                let owner = encode(&self.owner);
                format!(
                    "{base}/pull/new/{owner}:{}...{owner}:{}",
                    encode(branch),
                    encode(target?)
                )
            }
            Flavor::Gitea => format!("{base}/compare/{}...{}", encode(target?), encode(branch)),
            Flavor::Bitbucket => format!(
                "{base}/pull-requests/new?source={}&t=1",
                encode_value(branch)
            ),
            Flavor::Other => return None,
        })
    }
}

/// 推送时远程在输出里给的「创建合并请求」链接（GitLab、Gitee、Gitea、Bitbucket 每次都给，GitHub
/// 推新分支时给）。有它就用它：自己搭的平台路径前面可能多一段，远程给的才准。
pub fn request_link(output: &str) -> Option<String> {
    const MARKS: [&str; 4] = [
        "/merge_requests/new",
        "/pull/new/",
        "/pull-requests/new",
        "/compare/",
    ];
    output
        .lines()
        .filter(|line| line.trim_start().starts_with("remote:"))
        .flat_map(|line| line.split_ascii_whitespace())
        .find(|word| {
            (word.starts_with("https://") || word.starts_with("http://"))
                && MARKS.iter().any(|mark| word.contains(mark))
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 从远程地址推出网页() {
        let base = |url: &str| web(url).map(|web| web.base);
        assert_eq!(
            base("git@github.com:finmoor/kuai.git").as_deref(),
            Some("https://github.com/finmoor/kuai")
        );
        assert_eq!(
            base("https://github.com/finmoor/kuai").as_deref(),
            Some("https://github.com/finmoor/kuai")
        );
        // ssh 的端口不是网页的端口；账号去掉
        assert_eq!(
            base("ssh://git@git.example.com:2222/group/sub/app.git").as_deref(),
            Some("https://git.example.com/group/sub/app")
        );
        // https 的端口留着，地址里的账号密码不带进网页地址
        let web_ = web("http://user:secret@10.0.0.5:3000/team/app.git/").unwrap();
        assert_eq!(web_.base, "http://10.0.0.5:3000/team/app");
        assert_eq!(web_.host, "10.0.0.5:3000");
        assert_eq!(web_.owner, "team");
        assert_eq!(
            web("git@gitlab.com:group/sub/app.git").unwrap().owner,
            "group/sub"
        );
        // 本机路径、盘符、file:// 没有网页
        assert_eq!(base("/srv/git/app.git"), None);
        assert_eq!(base(r"C:\src\app"), None);
        assert_eq!(base("C:/src/app"), None);
        assert_eq!(base("file:///srv/git/app.git"), None);
        assert_eq!(base("../app"), None);
        assert_eq!(base("git@host:app.git"), None);
    }

    #[test]
    fn 公开的站点直接认得() {
        assert_eq!(Flavor::known("GitHub.com"), Some(Flavor::GitHub));
        assert_eq!(Flavor::known("gitlab.com"), Some(Flavor::GitLab));
        assert_eq!(Flavor::known("gitee.com"), Some(Flavor::Gitee));
        assert_eq!(Flavor::known("bitbucket.org"), Some(Flavor::Bitbucket));
        assert_eq!(Flavor::known("codeberg.org"), Some(Flavor::Gitea));
        assert_eq!(Flavor::known("git.example.com"), None);
        assert_eq!(Flavor::known("10.0.0.5:3000"), None);
    }

    #[test]
    fn 各平台的链接() {
        let web = web("git@h.example:team/app.git").unwrap();
        let (id, branch) = ("0123abcd", "feature/登录 #1");
        let enc = "feature/%E7%99%BB%E5%BD%95%20%231";
        assert_eq!(
            web.commit(Flavor::GitHub, id).unwrap(),
            "https://h.example/team/app/commit/0123abcd"
        );
        assert_eq!(
            web.commit(Flavor::GitLab, id).unwrap(),
            "https://h.example/team/app/-/commit/0123abcd"
        );
        assert_eq!(
            web.commit(Flavor::Bitbucket, id).unwrap(),
            "https://h.example/team/app/commits/0123abcd"
        );
        assert_eq!(web.commit(Flavor::Other, id), None);
        assert_eq!(
            web.branch(Flavor::Gitea, branch).unwrap(),
            format!("https://h.example/team/app/src/branch/{enc}")
        );
        assert_eq!(
            web.branch(Flavor::GitLab, branch).unwrap(),
            format!("https://h.example/team/app/-/tree/{enc}")
        );
        assert_eq!(
            web.new_request(Flavor::GitHub, branch, None).unwrap(),
            format!("https://h.example/team/app/compare/{enc}?expand=1")
        );
        assert_eq!(
            web.new_request(Flavor::GitHub, "dev", Some("main"))
                .unwrap(),
            "https://h.example/team/app/compare/main...dev?expand=1"
        );
        // 查询参数里的 / 也转义
        assert_eq!(
            web.new_request(Flavor::GitLab, "feature/x", Some("main"))
                .unwrap(),
            "https://h.example/team/app/-/merge_requests/new?merge_request%5Bsource_branch%5D=feature%2Fx&merge_request%5Btarget_branch%5D=main"
        );
        assert_eq!(
            web.new_request(Flavor::Gitee, "dev", Some("master"))
                .unwrap(),
            "https://h.example/team/app/pull/new/team:dev...team:master"
        );
        assert_eq!(
            web.new_request(Flavor::Gitea, "dev", Some("main")).unwrap(),
            "https://h.example/team/app/compare/main...dev"
        );
        // 不知道合并到哪个分支：这两家拼不出来
        assert_eq!(web.new_request(Flavor::Gitea, "dev", None), None);
        assert_eq!(web.new_request(Flavor::Gitee, "dev", None), None);
        assert_eq!(
            web.new_request(Flavor::Bitbucket, "feature/x", None)
                .unwrap(),
            "https://h.example/team/app/pull-requests/new?source=feature%2Fx&t=1"
        );
        assert_eq!(web.new_request(Flavor::Other, "dev", Some("main")), None);
    }

    #[test]
    fn 推送的输出里远程给的链接() {
        let gitlab = "remote: \nremote: To create a merge request for feature/x, visit:\nremote:   https://gitlab.example.com/g/app/-/merge_requests/new?merge_request%5Bsource_branch%5D=feature%2Fx\nremote: \nTo gitlab.example.com:g/app.git\n * [new branch]      feature/x -> feature/x\n";
        assert_eq!(
            request_link(gitlab).as_deref(),
            Some(
                "https://gitlab.example.com/g/app/-/merge_requests/new?merge_request%5Bsource_branch%5D=feature%2Fx"
            )
        );
        let github = "remote: Create a pull request for 'dev' on GitHub by visiting:\nremote:      https://github.com/o/r/pull/new/dev\n";
        assert_eq!(
            request_link(github).as_deref(),
            Some("https://github.com/o/r/pull/new/dev")
        );
        let gitea = "remote: Create a new pull request for 'dev':\nremote:   http://10.0.0.5:3000/team/app/compare/main...dev\n";
        assert_eq!(
            request_link(gitea).as_deref(),
            Some("http://10.0.0.5:3000/team/app/compare/main...dev")
        );
        // 不是远程说的、不是创建合并请求的链接不算
        assert_eq!(
            request_link(
                "To https://github.com/o/r/compare/x\nremote: see https://docs.example/help\n"
            ),
            None
        );
    }
}
