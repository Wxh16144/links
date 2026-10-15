//! 从本地 git 仓库构建 `ref -> URL` 索引。
//!
//! commit message 就是跳转目标，因此 `commit sha -> message` 天然构成映射；
//! tag 名是另一个入口，指向的 commit 的 message 才是目标 URL。
//! 仓库由宿主提供（clone / pull 都在容器外面做），这里只读。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// 一次 ref 解析的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// commit 短链：message 不可变，可永久重定向。
    Permanent(String),
    /// tag 短链：tag 可以移动，只能临时重定向。
    Temporary(String),
    NotFound,
    /// 前缀命中了多条 commit，数量见内层。
    Ambiguous(usize),
}

/// POST 详情查询的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum Detail {
    /// 命中 tag，附 tag 名与指向的 commit。
    Tag { tag: String, sha: String, url: String },
    /// 命中 commit（sha 前缀唯一）。
    Commit { sha: String, url: String },
}

struct TagInfo {
    sha: String,
    url: String,
}

pub struct Index {
    ref_name: String,
    loaded_at: u64,
    min_prefix_len: usize,
    /// `(sha, url)`，按 sha 升序排列以便前缀二分定位。
    commits: Vec<(String, String)>,
    /// `tag 名 -> (指向的 sha, 目标 url)`
    tags: HashMap<String, TagInfo>,
}

impl Index {
    pub fn build(
        ref_name: String,
        commits_raw: &[(String, String)],
        tags_raw: &[(String, String)],
        min_prefix_len: usize,
        strict_scheme: bool,
    ) -> Index {
        let mut commits = Vec::with_capacity(commits_raw.len());
        for (sha, message) in commits_raw {
            let url = message.trim();
            if !is_redirectable(url, strict_scheme) {
                eprintln!("[repo] 跳过 commit {sha}: message 不是可用 URL: {url:?}");
                continue;
            }
            commits.push((sha.to_ascii_lowercase(), url.to_owned()));
        }
        commits.sort();
        // 同一 commit 可能既是 ref 历史又是 tag 目标，去重避免误判歧义
        commits.dedup_by(|a, b| a.0 == b.0);

        let url_by_sha: HashMap<&str, &str> = commits
            .iter()
            .map(|(sha, url)| (sha.as_str(), url.as_str()))
            .collect();
        let mut tags = HashMap::with_capacity(tags_raw.len());
        for (tag, sha) in tags_raw {
            match url_by_sha.get(sha.as_str()) {
                Some(url) => {
                    tags.insert(
                        tag.clone(),
                        TagInfo {
                            sha: sha.clone(),
                            url: (*url).to_owned(),
                        },
                    );
                }
                None => eprintln!("[repo] 跳过 tag {tag}: 指向的 commit 不是短链 commit"),
            }
        }

        Index {
            ref_name,
            loaded_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
            min_prefix_len,
            commits,
            tags,
        }
    }

    /// tag 名优先于 SHA 前缀：显式命名胜过隐式派生。
    pub fn lookup(&self, raw: &str) -> Lookup {
        if let Some(info) = self.tags.get(raw) {
            return Lookup::Temporary(info.url.clone());
        }

        let prefix = raw.to_ascii_lowercase();
        if prefix.len() < self.min_prefix_len || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Lookup::NotFound;
        }

        let start = self
            .commits
            .partition_point(|(sha, _)| sha.as_str() < prefix.as_str());
        let hits: Vec<&str> = self.commits[start..]
            .iter()
            .take_while(|(sha, _)| sha.starts_with(&prefix))
            .map(|(_, url)| url.as_str())
            .collect();

        match hits.len() {
            0 => Lookup::NotFound,
            1 => Lookup::Permanent(hits[0].to_owned()),
            n => Lookup::Ambiguous(n),
        }
    }

    /// POST 用：返回 reference 指向的 commit 详情；歧义或未命中返回 `None`。
    pub fn detail(&self, raw: &str) -> Option<Detail> {
        if let Some(info) = self.tags.get(raw) {
            return Some(Detail::Tag {
                tag: raw.to_owned(),
                sha: info.sha.clone(),
                url: info.url.clone(),
            });
        }

        let prefix = raw.to_ascii_lowercase();
        if prefix.len() < self.min_prefix_len || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }

        let start = self
            .commits
            .partition_point(|(sha, _)| sha.as_str() < prefix.as_str());
        let mut hits = self.commits[start..]
            .iter()
            .take_while(|(sha, _)| sha.starts_with(&prefix));
        let first = hits.next()?;
        if hits.next().is_some() {
            return None;
        }
        Some(Detail::Commit {
            sha: first.0.clone(),
            url: first.1.clone(),
        })
    }

    pub fn ref_name(&self) -> &str {
        &self.ref_name
    }

    pub fn commit_count(&self) -> usize {
        self.commits.len()
    }

    pub fn tag_count(&self) -> usize {
        self.tags.len()
    }

    pub fn loaded_at(&self) -> u64 {
        self.loaded_at
    }
}

/// 宿主挂载进来的仓库；服务只读，不 clone、不 fetch。
pub struct Repo {
    dir: PathBuf,
    git_ref: Option<String>,
}

impl Repo {
    pub fn new(dir: PathBuf, git_ref: Option<String>) -> Self {
        Self { dir, git_ref }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 重新扫描仓库并重建索引。
    pub fn load(&self, min_prefix_len: usize, strict_scheme: bool) -> Result<Index, String> {
        if !is_git_repo(&self.dir) {
            return Err(format!(
                "{} 不是 git 仓库（宿主要先 clone / pull 好再挂进来）",
                self.dir.display()
            ));
        }

        let ref_name = self.resolve_ref();
        let mut commits = read_commits(&self.dir, &ref_name)?;

        // tag 可能指向不在本 ref 历史里的 commit，单独补齐
        let tags = read_tags(&self.dir)?;
        let known: HashSet<&str> = commits.iter().map(|(sha, _)| sha.as_str()).collect();
        let mut missing: Vec<&str> = tags
            .iter()
            .map(|(_, sha)| sha.as_str())
            .filter(|sha| !known.contains(sha))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        commits.extend(read_commits_by_sha(&self.dir, &missing)?);

        Ok(Index::build(
            ref_name,
            &commits,
            &tags,
            min_prefix_len,
            strict_scheme,
        ))
    }

    fn resolve_ref(&self) -> String {
        if let Some(explicit) = self.git_ref.as_deref().filter(|r| !r.trim().is_empty()) {
            return explicit.to_owned();
        }
        if let Ok(out) = run_git(&self.dir, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
            && !out.trim().is_empty()
        {
            return out.trim().to_owned();
        }
        "HEAD".to_owned()
    }
}

/// `strict` 时只接受 http/https；无论严格与否都必须能安全放进 `Location` 头。
fn is_redirectable(url: &str, strict: bool) -> bool {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if !strict {
        return true;
    }
    url.starts_with("http://") || url.starts_with("https://")
}

fn is_git_repo(dir: &Path) -> bool {
    dir.is_dir() && run_git(dir, &["rev-parse", "--git-dir"]).is_ok()
}

fn read_commits(dir: &Path, ref_name: &str) -> Result<Vec<(String, String)>, String> {
    let out = run_git(dir, &["log", "--format=%H%x09%s", ref_name])?;
    Ok(parse_commit_rows(&out))
}

fn read_commits_by_sha(dir: &Path, shas: &[&str]) -> Result<Vec<(String, String)>, String> {
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["log", "--no-walk=unsorted", "--format=%H%x09%s"];
    args.extend_from_slice(shas);
    let out = run_git(dir, &args)?;
    Ok(parse_commit_rows(&out))
}

fn parse_commit_rows(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(sha, message)| (sha.to_ascii_lowercase(), message.trim().to_owned()))
        .collect()
}

/// 返回 `(tag 名, 指向的 commit sha)`；ref 名不允许含空格，故只用空格分隔。
fn read_tags(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let out = run_git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname:short) %(objectname) %(*objectname)",
            "refs/tags",
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let tag = parts.next()?;
            let object = parts.next()?;
            // 附注 tag 的 peeled 值才是 commit，轻量 tag 直接就是 commit
            let peeled = parts.next().unwrap_or("").trim();
            let sha = if peeled.is_empty() { object } else { peeled };
            Some((tag.to_owned(), sha.to_ascii_lowercase()))
        })
        .collect())
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    // 容器里挂载的仓库常常属于另一个 uid
    cmd.arg("-C")
        .arg(dir)
        .arg("-c")
        .arg("safe.directory=*")
        .args(args);

    let output = cmd
        .output()
        .map_err(|e| format!("执行 git 失败: {e}（镜像里装了 git 吗？）"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} 失败: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commits(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(sha, url)| ((*sha).to_owned(), (*url).to_owned()))
            .collect()
    }

    fn index(commits_raw: Vec<(String, String)>, tags_raw: Vec<(String, String)>) -> Index {
        Index::build("test".to_owned(), &commits_raw, &tags_raw, 4, true)
    }

    #[test]
    fn unique_prefix_redirects_permanently() {
        let idx = index(
            commits(&[
                ("aaaa1111", "https://example.com/a"),
                ("bbbb2222", "https://example.com/b"),
            ]),
            vec![],
        );
        assert_eq!(
            idx.lookup("aaaa"),
            Lookup::Permanent("https://example.com/a".to_owned())
        );
        // 全 SHA 也要能命中
        assert_eq!(
            idx.lookup("bbbb2222"),
            Lookup::Permanent("https://example.com/b".to_owned())
        );
        // 大小写不敏感
        assert_eq!(
            idx.lookup("AAAA"),
            Lookup::Permanent("https://example.com/a".to_owned())
        );
    }

    #[test]
    fn prefix_shorter_than_minimum_is_not_found() {
        let idx = index(
            commits(&[
                ("ab111111", "https://example.com/a"),
                ("ab222222", "https://example.com/b"),
            ]),
            vec![],
        );
        assert_eq!(idx.lookup("ab"), Lookup::NotFound);
        assert_eq!(idx.lookup("ab1"), Lookup::NotFound);
        assert_eq!(
            idx.lookup("ab11"),
            Lookup::Permanent("https://example.com/a".to_owned())
        );
    }

    #[test]
    fn ambiguous_prefix_is_reported_not_silently_resolved() {
        let idx = index(
            commits(&[
                ("abcd1111", "https://example.com/a"),
                ("abcd2222", "https://example.com/b"),
            ]),
            vec![],
        );
        assert_eq!(idx.lookup("abcd"), Lookup::Ambiguous(2));
    }

    #[test]
    fn non_hex_and_unknown_refs_are_not_found() {
        let idx = index(commits(&[("aaaa1111", "https://example.com/a")]), vec![]);
        assert_eq!(idx.lookup("zzzz"), Lookup::NotFound);
        assert_eq!(idx.lookup("aaaa1111x"), Lookup::NotFound);
        assert_eq!(idx.lookup(""), Lookup::NotFound);
    }

    #[test]
    fn tag_wins_over_sha_prefix() {
        let idx = index(
            commits(&[("abcd1111", "https://example.com/commit")]),
            vec![("abcd".to_owned(), "abcd1111".to_owned())],
        );
        assert_eq!(
            idx.lookup("abcd"),
            Lookup::Temporary("https://example.com/commit".to_owned())
        );
    }

    #[test]
    fn tag_shorter_than_min_prefix_still_resolves() {
        let idx = index(
            commits(&[("abcd1111", "https://example.com/commit")]),
            vec![("ab".to_owned(), "abcd1111".to_owned())],
        );
        assert_eq!(
            idx.lookup("ab"),
            Lookup::Temporary("https://example.com/commit".to_owned())
        );
    }

    #[test]
    fn tag_pointing_at_non_shortlink_commit_is_dropped() {
        let idx = index(
            commits(&[("aaaa1111", "chore: not a url")]),
            vec![("named".to_owned(), "aaaa1111".to_owned())],
        );
        assert_eq!(idx.lookup("named"), Lookup::NotFound);
        assert_eq!(idx.commit_count(), 0);
    }

    #[test]
    fn duplicate_commits_are_deduped() {
        // 同一 commit 同时来自 ref 历史和 tag 目标，不应算作歧义
        let idx = index(
            commits(&[
                ("abcd1111", "https://example.com/a"),
                ("abcd1111", "https://example.com/a"),
            ]),
            vec![],
        );
        assert_eq!(idx.commit_count(), 1);
        assert_eq!(
            idx.lookup("abcd"),
            Lookup::Permanent("https://example.com/a".to_owned())
        );
    }

    #[test]
    fn strict_scheme_filters_dangerous_messages() {
        let raw = commits(&[
            ("aaaa1111", "javascript:alert(1)"),
            ("bbbb2222", "data:text/html,<script>x</script>"),
            ("cccc3333", "https://example.com/ok"),
            ("dddd4444", "http://example.com/insecure-ok"),
        ]);
        let strict = Index::build("test".to_owned(), &raw, &[], 4, true);
        assert_eq!(strict.commit_count(), 2);
        assert_eq!(strict.lookup("aaaa"), Lookup::NotFound);
        assert_eq!(strict.lookup("bbbb"), Lookup::NotFound);

        let loose = Index::build("test".to_owned(), &raw, &[], 4, false);
        assert_eq!(loose.commit_count(), 4);
        assert_eq!(
            loose.lookup("aaaa"),
            Lookup::Permanent("javascript:alert(1)".to_owned())
        );
    }

    #[test]
    fn urls_with_whitespace_are_rejected_even_when_loose() {
        let raw = commits(&[
            ("aaaa1111", "https://example.com/a b"),
            ("bbbb2222", "https://example.com/\nheader"),
        ]);
        let loose = Index::build("test".to_owned(), &raw, &[], 4, false);
        assert_eq!(loose.commit_count(), 0);
    }
}
