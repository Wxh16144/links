//! 短链服务：把 `/<ref>` 解析成 commit message 里的 URL 并重定向。
//!
//! 索引完全来自本地 git 仓库，不访问 GitHub API。

mod repo;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use repo::{Detail, Index, Lookup, Repo};

const ROOT_HTML: &str = include_str!("../assets/root.html");
const ERROR_HTML: &str = include_str!("../assets/error.html");
const HEALTH_HTML: &str = include_str!("../assets/health.html");

struct Config {
    listen_addr: SocketAddr,
    refresh_interval: Duration,
    min_prefix_len: usize,
    strict_scheme: bool,
    repo_dir: PathBuf,
    git_ref: Option<String>,
}

impl Config {
    fn from_env(repo_dir_arg: Option<String>) -> Result<Self, String> {
        // 命令行第一个参数优先，其次 REPO_DIR，最后默认 /repo
        let repo_dir = repo_dir_arg
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("REPO_DIR")
                    .ok()
                    .filter(|d| !d.trim().is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("/repo"));

        Ok(Config {
            listen_addr: listen_addr()?,
            refresh_interval: parse_duration(&env_str("REFRESH_INTERVAL", "5m"))?,
            min_prefix_len: env_str("MIN_PREFIX_LEN", "4")
                .parse()
                .map_err(|e| format!("MIN_PREFIX_LEN 无效: {e}"))?,
            strict_scheme: env_bool("STRICT_SCHEME", true),
            repo_dir,
            git_ref: std::env::var("GIT_REF").ok().filter(|r| !r.trim().is_empty()),
        })
    }
}

/// 只暴露端口；默认绑定 localhost，需要对外时靠反向代理 / 端口转发。
fn listen_addr() -> Result<SocketAddr, String> {
    let port = match std::env::var("PORT").ok().filter(|v| !v.trim().is_empty()) {
        Some(v) => v.trim().parse().map_err(|_| format!("PORT 无效: {v}"))?,
        None => 8080,
    };
    Ok(SocketAddr::from(([127, 0, 0, 1], port)))
}

struct App {
    repo: Repo,
    refresh_interval: Duration,
    min_prefix_len: usize,
    strict_scheme: bool,
    started_at: std::time::Instant,
    index: RwLock<Option<Arc<Index>>>,
    last_error: RwLock<Option<String>>,
}

impl App {
    /// 会 spawn git，调用方负责放到阻塞线程上。
    fn reload(&self) {
        match self.repo.load(self.min_prefix_len, self.strict_scheme) {
            Ok(index) => {
                eprintln!(
                    "[links] 已加载 {} 条 commit、{} 个 tag（ref={}）",
                    index.commit_count(),
                    index.tag_count(),
                    index.ref_name()
                );
                *self.index.write().unwrap() = Some(Arc::new(index));
                *self.last_error.write().unwrap() = None;
            }
            Err(e) => {
                eprintln!("[links] 重新加载失败: {e}");
                *self.last_error.write().unwrap() = Some(e);
            }
        }
    }

    fn current(&self) -> Option<Arc<Index>> {
        self.index.read().unwrap().clone()
    }

    fn last_error(&self) -> String {
        self.last_error
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "unknown".to_owned())
            .replace('\n', " ")
    }
}

async fn reload(app: Arc<App>) {
    if let Err(e) = tokio::task::spawn_blocking(move || app.reload()).await {
        eprintln!("[links] 加载任务异常: {e}");
    }
}

#[tokio::main]
async fn main() {
    let repo_dir_arg = std::env::args().nth(1);
    let config = match Config::from_env(repo_dir_arg) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("[links] 配置错误: {e}");
            std::process::exit(2);
        }
    };
    let listen_addr = config.listen_addr;

    let app = Arc::new(App {
        refresh_interval: config.refresh_interval,
        min_prefix_len: config.min_prefix_len,
        strict_scheme: config.strict_scheme,
        started_at: std::time::Instant::now(),
        repo: Repo::new(config.repo_dir, config.git_ref),
        index: RwLock::new(None),
        last_error: RwLock::new(None),
    });

    // 首次加载失败也不退出：健康检查报 503，后台继续重试
    reload(app.clone()).await;

    spawn_refresher(app.clone());
    spawn_reload_on_sighup(app.clone());

    let router = Router::new()
        .route("/", get(root))
        .route("/_health", get(health))
        // tag 名可以含 `/`（例如 me/antd），所以必须用通配而不是单段参数
        .route("/{*reference}", get(redirect).post(detail))
        .with_state(app)
        .layer(middleware::from_fn(log_request));

    let listener = match TcpListener::bind(listen_addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[links] 监听 {listen_addr} 失败: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[links] 监听 http://{listen_addr}");

    if let Err(e) = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("[links] 服务退出: {e}");
    }
}

fn spawn_refresher(app: Arc<App>) {
    let interval = app.refresh_interval;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // 首个 tick 立即完成，跳过
        loop {
            ticker.tick().await;
            reload(app.clone()).await;
        }
    });
}

fn spawn_reload_on_sighup(app: Arc<App>) {
    let Ok(mut hangup) = signal(SignalKind::hangup()) else {
        eprintln!("[links] 无法注册 SIGHUP");
        return;
    };
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            eprintln!("[links] 收到 SIGHUP，重新加载");
            reload(app.clone()).await;
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

async fn log_request(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let start = std::time::Instant::now();
    let response = next.run(req).await;
    eprintln!(
        "[links] {} {} -> {} ({}ms)",
        method,
        path,
        response.status().as_u16(),
        start.elapsed().as_millis()
    );
    response
}

async fn root() -> Response {
    html(StatusCode::OK, ROOT_HTML.to_owned())
}

async fn health(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let uptime = app.started_at.elapsed().as_secs_f64();
    // 一个 URL 两种表现：浏览器要 HTML 页面，curl / 探针拿 JSON
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/html"))
        .unwrap_or(false);

    match app.current() {
        Some(index) if wants_html => {
            let body = HEALTH_HTML
                .replace("{{ref}}", index.ref_name())
                .replace("{{repo}}", &app.repo.dir().display().to_string())
                .replace("{{commits}}", &index.commit_count().to_string())
                .replace("{{tags}}", &index.tag_count().to_string())
                .replace("{{loaded_at}}", &index.loaded_at().to_string())
                .replace("{{uptime}}", &uptime.to_string());
            html(StatusCode::OK, body)
        }
        Some(index) => json(
            StatusCode::OK,
            format!(
                r#"{{"status":"ok","uptime":{},"ref":"{}","repo":"{}","commits":{},"tags":{},"loaded_at":{}}}"#,
                uptime.floor() as u64,
                json_escape(index.ref_name()),
                json_escape(&app.repo.dir().display().to_string()),
                index.commit_count(),
                index.tag_count(),
                index.loaded_at()
            ),
        ),
        None => json(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                r#"{{"status":"not ready","uptime":{},"error":"{}"}}"#,
                uptime.floor() as u64,
                json_escape(&app.last_error())
            ),
        ),
    }
}

async fn redirect(State(app): State<Arc<App>>, Path(reference): Path<String>) -> Response {
    let Some(index) = app.current() else {
        return error_page(StatusCode::SERVICE_UNAVAILABLE, "Service is starting up");
    };

    match index.lookup(reference.trim_matches('/')) {
        // commit 短链的 message 不可变，可以长期缓存
        Lookup::Permanent(url) => location(StatusCode::MOVED_PERMANENTLY, url),
        // tag 可以移动，必须让客户端每次回源
        Lookup::Temporary(url) => location(StatusCode::FOUND, url),
        Lookup::Ambiguous(count) => error_page(
            StatusCode::CONFLICT,
            &format!("Ambiguous prefix, matches {count} commits"),
        ),
        Lookup::NotFound => error_page(StatusCode::NOT_FOUND, "Short link not found"),
    }
}

async fn detail(State(app): State<Arc<App>>, Path(reference): Path<String>) -> Response {
    let Some(index) = app.current() else {
        return json(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"Service is starting up"}"#.to_owned(),
        );
    };

    match index.detail(reference.trim_matches('/')) {
        Some(Detail::Commit { sha, url }) => json(
            StatusCode::OK,
            format!(
                r#"{{"kind":"commit","sha":"{sha}","url":"{}"}}"#,
                json_escape(&url)
            ),
        ),
        Some(Detail::Tag { tag, sha, url }) => json(
            StatusCode::OK,
            format!(
                r#"{{"kind":"tag","tag":"{}","sha":"{sha}","url":"{}"}}"#,
                json_escape(&tag),
                json_escape(&url)
            ),
        ),
        None => json(StatusCode::NOT_FOUND, r#"{"kind":"none"}"#.to_owned()),
    }
}

fn location(code: StatusCode, url: String) -> Response {
    if let Ok(value) = HeaderValue::from_str(&url) {
        return (code, [(header::LOCATION, value)]).into_response();
    }
    // 含非 ASCII 的 URL 必须百分号编码才放得进 header
    match HeaderValue::from_str(&percent_encode(&url)) {
        Ok(value) => (code, [(header::LOCATION, value)]).into_response(),
        Err(_) => error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored URL cannot be sent as a header",
        ),
    }
}

fn html(code: StatusCode, body: String) -> Response {
    (
        code,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

fn json(code: StatusCode, body: String) -> Response {
    (
        code,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// 手写 JSON，只转义会破坏结构的字符。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `message` 里不放用户输入，所以可以直接替换。
fn error_page(code: StatusCode, message: &str) -> Response {
    html(code, ERROR_HTML.replace("{{message}}", message))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if (0x21..=0x7e).contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let (value, unit) = match s.chars().last() {
        Some(unit) if unit.is_ascii_alphabetic() => (&s[..s.len() - 1], unit),
        _ => (s, 's'),
    };
    let value: u64 = value
        .trim()
        .parse()
        .map_err(|_| format!("REFRESH_INTERVAL 无效: {s:?}"))?;
    let seconds = match unit {
        's' => value,
        'm' => value * 60,
        'h' => value * 3600,
        _ => return Err(format!("REFRESH_INTERVAL 单位无效: {s:?}")),
    };
    Ok(Duration::from_secs(seconds))
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_support_suffixes() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn percent_encoding_keeps_ascii_urls_intact() {
        let url = "https://example.com/a?b=1&c=2#frag";
        assert_eq!(percent_encode(url), url);
    }

    #[test]
    fn percent_encoding_escapes_non_ascii() {
        assert_eq!(
            percent_encode("https://例子.com/路径"),
            "https://%E4%BE%8B%E5%AD%90.com/%E8%B7%AF%E5%BE%84"
        );
    }
}
