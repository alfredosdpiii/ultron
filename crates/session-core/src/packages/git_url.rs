//! `parseGitUrl` (utils/git.ts) with the part of `hosted-git-info` 9 it relies on (`fromUrl` for GitHub, GitLab,
//! Bitbucket, gists and sourcehut, shortcuts such as `github:user/repo` and `user/repo`), over WHATWG URLs.
//!
//! Evidence: `crates/cli/fixtures/packages.mjs` records `parseGitUrl` over a list of sources;
//! `tests/packages_units.rs` replays it.

use url::Url;

/// A parsed git source (`GitSource`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSource {
    /// The clone URL, without a ref suffix.
    pub repo: String,
    pub host: String,
    /// `user/repo`.
    pub path: String,
    pub git_ref: Option<String>,
    pub pinned: bool,
}

/// `decodeURIComponent`; None where it throws (a malformed escape or invalid UTF-8).
pub fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `splitRef`: a trailing `@ref` on the repository path.
fn split_ref(url: &str) -> (String, Option<String>) {
    let whole = || (url.to_string(), None);
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
        && !host.is_empty()
        && !path.is_empty()
    {
        let Some((repo_path, git_ref)) = path.split_once('@') else {
            return whole();
        };
        if repo_path.is_empty() || git_ref.is_empty() {
            return whole();
        }
        return (format!("git@{host}:{repo_path}"), Some(git_ref.to_string()));
    }
    if url.contains("://") {
        let Ok(mut parsed) = Url::parse(url) else {
            return whole();
        };
        let path = parsed.path().trim_start_matches('/').to_string();
        let Some((repo_path, git_ref)) = path.split_once('@') else {
            return whole();
        };
        if repo_path.is_empty() || git_ref.is_empty() {
            return whole();
        }
        parsed.set_path(&format!("/{repo_path}"));
        let text = parsed.to_string();
        let repo = text.strip_suffix('/').unwrap_or(&text).to_string();
        return (repo, Some(git_ref.to_string()));
    }
    let Some(slash) = url.find('/') else {
        return whole();
    };
    let (host, path) = (&url[..slash], &url[slash + 1..]);
    let Some((repo_path, git_ref)) = path.split_once('@') else {
        return whole();
    };
    if repo_path.is_empty() || git_ref.is_empty() {
        return whole();
    }
    (format!("{host}/{repo_path}"), Some(git_ref.to_string()))
}

/// `hasUnsafeGitInstallPart`.
fn unsafe_part(value: &str, allow_slash: bool) -> bool {
    let Some(decoded) = decode_uri_component(value) else {
        return true;
    };
    [value, decoded.as_str()].iter().any(|candidate| {
        candidate.contains('\0')
            || candidate.contains('\\')
            || candidate.starts_with('/')
            || (!allow_slash && candidate.contains('/'))
            || candidate.split('/').any(|s| s == "..")
    })
}

/// `buildGitSource`.
fn build(repo: String, host: &str, path: &str, git_ref: Option<String>) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let path = path.strip_suffix(".git").unwrap_or(path).trim_start_matches('/');
    if host.is_empty() || path.is_empty() || path.split('/').count() < 2 {
        return None;
    }
    if unsafe_part(host, false) || unsafe_part(path, true) {
        return None;
    }
    Some(GitSource {
        repo,
        host: host.to_string(),
        path: path.to_string(),
        pinned: git_ref.is_some(),
        git_ref,
    })
}

/// `parseGenericGitUrl`.
fn parse_generic(url: &str) -> Option<GitSource> {
    let (without_ref, git_ref) = split_ref(url);
    let mut repo = without_ref.clone();
    let (host, path);
    if let Some(rest) = without_ref.strip_prefix("git@")
        && let Some((h, p)) = rest.split_once(':')
        && !h.is_empty()
        && !p.is_empty()
    {
        host = h.to_string();
        path = p.to_string();
    } else if ["https://", "http://", "ssh://", "git://"]
        .iter()
        .any(|s| without_ref.starts_with(s))
    {
        let parsed = Url::parse(&without_ref).ok()?;
        host = parsed.host_str().unwrap_or_default().to_string();
        path = parsed.path().trim_start_matches('/').to_string();
    } else {
        let slash = without_ref.find('/')?;
        host = without_ref[..slash].to_string();
        path = without_ref[slash + 1..].to_string();
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        repo = format!("https://{without_ref}");
    }
    build(repo, &host, &path, git_ref)
}

// ---------------------------------------------------------------------------------------------- hosted-git-info

#[derive(Clone, Copy, PartialEq, Eq)]
enum Host {
    GitHub,
    Bitbucket,
    GitLab,
    Gist,
    Sourcehut,
}

impl Host {
    fn domain(self) -> &'static str {
        match self {
            Host::GitHub => "github.com",
            Host::Bitbucket => "bitbucket.org",
            Host::GitLab => "gitlab.com",
            Host::Gist => "gist.github.com",
            Host::Sourcehut => "git.sr.ht",
        }
    }

    fn protocols(self) -> &'static [&'static str] {
        match self {
            Host::GitHub => &["git:", "http:", "git+ssh:", "git+https:", "ssh:", "https:"],
            Host::Bitbucket | Host::GitLab => &["git+ssh:", "git+https:", "ssh:", "https:"],
            Host::Gist => &["git:", "git+ssh:", "git+https:", "ssh:", "https:"],
            Host::Sourcehut => &["git+ssh:", "https:"],
        }
    }

    fn by_shortcut(protocol: &str) -> Option<Host> {
        Some(match protocol {
            "github:" => Host::GitHub,
            "bitbucket:" => Host::Bitbucket,
            "gitlab:" => Host::GitLab,
            "gist:" => Host::Gist,
            "sourcehut:" => Host::Sourcehut,
            _ => return None,
        })
    }

    fn by_domain(hostname: &str) -> Option<Host> {
        let hostname = hostname.strip_prefix("www.").unwrap_or(hostname);
        [Host::GitHub, Host::Bitbucket, Host::GitLab, Host::Gist, Host::Sourcehut]
            .into_iter()
            .find(|h| h.domain() == hostname)
    }
}

/// The protocols `hosted-git-info` knows (its own and the hosts' shortcuts).
fn known_protocol(protocol: &str) -> bool {
    [
        "git+ssh:",
        "ssh:",
        "git+https:",
        "git:",
        "http:",
        "https:",
        "git+http:",
        "github:",
        "bitbucket:",
        "gitlab:",
        "gist:",
        "sourcehut:",
    ]
    .contains(&protocol)
}

/// What `fromUrl` keeps: the host and the parsed user, project and committish.
struct Hosted {
    host: Host,
    user: Option<String>,
    project: String,
    committish: Option<String>,
}

/// `isGitHubShorthand`.
fn is_github_shorthand(arg: &str) -> bool {
    let index = |c: char| arg.find(c);
    let first_hash = index('#');
    let first_slash = index('/');
    let second_slash = first_slash.and_then(|s| arg[s + 1..].find('/').map(|i| i + s + 1));
    let first_colon = index(':');
    let first_space = arg.find(char::is_whitespace);
    let first_at = index('@');
    let after_hash = |at: Option<usize>| match at {
        None => true,
        Some(at) => first_hash.is_some_and(|h| at > h),
    };
    let does_not_end_with_slash = match first_hash {
        Some(h) => h == 0 || arg.as_bytes()[h - 1] != b'/',
        None => !arg.ends_with('/'),
    };
    after_hash(first_space)
        && first_slash.is_some_and(|s| s > 0)
        && does_not_end_with_slash
        && !arg.starts_with('.')
        && after_hash(first_at)
        && after_hash(first_colon)
        && after_hash(second_slash)
}

/// `lastIndexOfBefore(str, char, beforeChar)`.
fn last_index_before(text: &str, c: char, before: char) -> Option<usize> {
    let end = text.find(before).unwrap_or(text.len());
    text[..end].rfind(c)
}

/// `correctProtocol`.
fn correct_protocol(arg: &str) -> String {
    let first_colon = arg.find(':');
    let proto = match first_colon {
        Some(c) => &arg[..c + 1],
        None => "",
    };
    if known_protocol(proto) {
        return arg.to_string();
    }
    if let Some(c) = first_colon
        && arg[c..].starts_with("://")
    {
        return arg.to_string();
    }
    if let Some(at) = arg.find('@') {
        return if first_colon.is_none_or(|c| at > c) {
            format!("git+ssh://{arg}")
        } else {
            arg.to_string()
        };
    }
    match first_colon {
        Some(c) => format!("{}//{}", &arg[..c + 1], &arg[c + 1..]),
        None => format!("//{arg}"),
    }
}

/// `correctUrl`: an scp-style address becomes a parseable URL.
fn correct_url(giturl: &str) -> String {
    let mut giturl = giturl.to_string();
    let first_at = last_index_before(&giturl, '@', '#');
    let last_colon = last_index_before(&giturl, ':', '#');
    let after = match (last_colon, first_at) {
        (Some(c), Some(a)) => c > a,
        (Some(_), None) => true,
        _ => false,
    };
    if after && let Some(c) = last_colon {
        giturl = format!("{}/{}", &giturl[..c], &giturl[c + 1..]);
    }
    if last_index_before(&giturl, ':', '#').is_none() && !giturl.contains("//") {
        giturl = format!("git+ssh://{giturl}");
    }
    giturl
}

fn parse_url(giturl: &str) -> Option<Url> {
    let with_protocol = correct_protocol(giturl);
    Url::parse(&with_protocol)
        .ok()
        .or_else(|| Url::parse(&correct_url(&with_protocol)).ok())
}

fn hash(url: &Url) -> String {
    url.fragment().unwrap_or_default().to_string()
}

/// `fromUrl`.
fn from_url(giturl: &str) -> Option<Hosted> {
    if giturl.is_empty() {
        return None;
    }
    let corrected = if is_github_shorthand(giturl) {
        format!("github:{giturl}")
    } else {
        giturl.to_string()
    };
    let parsed = parse_url(&corrected)?;
    let protocol = format!("{}:", parsed.scheme());
    let hostname = parsed.host_str().unwrap_or_default();
    let shortcut = Host::by_shortcut(&protocol);
    let host = shortcut.or_else(|| Host::by_domain(hostname))?;
    if shortcut.is_some() {
        let mut pathname = parsed.path().strip_prefix('/').unwrap_or(parsed.path()).to_string();
        if let Some(at) = pathname.find('@') {
            pathname = pathname[at + 1..].to_string();
        }
        let (user, project) = match pathname.rfind('/') {
            Some(slash) => {
                let user = decode_uri_component(&pathname[..slash])?;
                (
                    Some(user).filter(|u| !u.is_empty()),
                    decode_uri_component(&pathname[slash + 1..])?,
                )
            }
            None => (None, decode_uri_component(&pathname)?),
        };
        let project = project.strip_suffix(".git").map(str::to_string).unwrap_or(project);
        let committish = match parsed.fragment() {
            Some(f) => Some(decode_uri_component(f)?),
            None => None,
        };
        return Some(Hosted {
            host,
            user,
            project,
            committish,
        });
    }
    if !host.protocols().contains(&protocol.as_str()) {
        return None;
    }
    let path = parsed.path();
    // `pathname.split('/', n)`: the leading empty segment, then the parts.
    let parts: Vec<&str> = path.split('/').collect();
    let part = |i: usize| parts.get(i).copied().filter(|p| !p.is_empty());
    let (user, project, committish): (Option<String>, String, String) = match host {
        Host::GitHub => {
            let (user, project, kind) = (part(1), part(2), parts.get(3).copied());
            if kind.is_some_and(|t| !t.is_empty() && t != "tree") {
                return None;
            }
            let committish = if kind.is_none_or(str::is_empty) {
                hash(&parsed)
            } else {
                // `decodeURIComponent(undefined)` is "undefined".
                parts.get(4).copied().unwrap_or("undefined").to_string()
            };
            let project = project.map(|p| p.strip_suffix(".git").unwrap_or(p));
            let (Some(user), Some(project)) = (user, project.filter(|p| !p.is_empty())) else {
                return None;
            };
            (Some(user.to_string()), project.to_string(), committish)
        }
        Host::Bitbucket | Host::Sourcehut => {
            let aux = parts.get(3).copied();
            let blocked = if host == Host::Bitbucket { "get" } else { "archive" };
            if aux == Some(blocked) {
                return None;
            }
            let project = part(2).map(|p| p.strip_suffix(".git").unwrap_or(p));
            let (Some(user), Some(project)) = (part(1), project.filter(|p| !p.is_empty())) else {
                return None;
            };
            (Some(user.to_string()), project.to_string(), hash(&parsed))
        }
        Host::GitLab => {
            let path = &path[1.min(path.len())..];
            if path.contains("/-/") || path.contains("/archive.tar.gz") {
                return None;
            }
            let mut segments: Vec<&str> = path.split('/').collect();
            let project = segments.pop().unwrap_or_default();
            let project = project.strip_suffix(".git").unwrap_or(project);
            let user = segments.join("/");
            if user.is_empty() || project.is_empty() {
                return None;
            }
            (Some(user), project.to_string(), hash(&parsed))
        }
        Host::Gist => {
            if parts.get(3).copied() == Some("raw") {
                return None;
            }
            let (user, project) = match (parts.get(1).copied(), part(2)) {
                (user, Some(project)) => (user, project),
                (Some(user), None) if !user.is_empty() => (None, user),
                _ => return None,
            };
            let project = project.strip_suffix(".git").unwrap_or(project);
            (user.map(str::to_string), project.to_string(), hash(&parsed))
        }
    };
    Some(Hosted {
        host,
        user: match user {
            Some(u) => Some(decode_uri_component(&u)?),
            None => None,
        },
        project: decode_uri_component(&project)?,
        committish: Some(decode_uri_component(&committish)?),
    })
}

/// `parseGitUrl`: with a `git:` prefix every historical shorthand; without one, only protocol URLs.
pub fn parse_git_url(source: &str) -> Option<GitSource> {
    let trimmed = source.trim();
    let has_prefix = trimmed.starts_with("git:");
    let url = if has_prefix { trimmed[4..].trim() } else { trimmed };
    let lower = url.to_ascii_lowercase();
    if !has_prefix
        && !["http://", "https://", "ssh://", "git://"]
            .iter()
            .any(|p| lower.starts_with(p))
    {
        return None;
    }
    let (repo, split) = split_ref(url);
    let with_ref = split.as_ref().map(|r| format!("{repo}#{r}"));
    let hosted = |candidate: &str, https: bool| -> Option<Option<GitSource>> {
        let info = from_url(candidate)?;
        if split.is_some() && info.project.contains('@') {
            return None;
        }
        let protocol_repo = ["http://", "https://", "ssh://", "git://", "git@"]
            .iter()
            .any(|p| repo.starts_with(p));
        let clone = if https || !protocol_repo {
            format!("https://{repo}")
        } else {
            repo.clone()
        };
        let user = info.user.clone().unwrap_or_else(|| "null".into());
        let git_ref = info
            .committish
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| split.clone());
        Some(build(
            clone,
            info.host.domain(),
            &format!("{user}/{}", info.project),
            git_ref,
        ))
    };
    for candidate in with_ref.iter().map(String::as_str).chain([url]) {
        if let Some(result) = hosted(candidate, false) {
            return result;
        }
    }
    let https_with_ref = split.as_ref().map(|r| format!("https://{repo}#{r}"));
    let https_plain = format!("https://{url}");
    for candidate in https_with_ref.iter().map(String::as_str).chain([https_plain.as_str()]) {
        if let Some(result) = hosted(candidate, true) {
            return result;
        }
    }
    parse_generic(url)
}
