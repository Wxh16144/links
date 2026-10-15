# links

短链服务：把 `/<ref>` 跳转到 git 仓库里对应 commit message 的 URL。数据全在本地 git 历史里，不调用 GitHub API。

## Run

```sh
git clone --branch urls https://github.com/Wxh16144/links.git /srv/links
```

### Docker

```sh
docker build -t wxh16144/links .
docker run -d --name links -p 8080:8080 \
  -v /srv/links:/repo:ro \
  wxh16144/links
```

### Cargo

```sh
cargo build --release
./target/release/links /srv/links
```

`/srv/links` 更新后会自动重扫；也可 `docker kill -s HUP links` 立刻重扫。

## Options

| 环境变量 | 默认 | 说明 |
| --- | --- | --- |
| `REPO_DIR` | `/repo` | 短链仓库路径（也可作为第一个命令行参数传入） |
| `GIT_REF` | `origin/HEAD` | 读取的 ref，兜底 `HEAD` |
| `PORT` | `8080` | 监听端口（默认绑定 `127.0.0.1`） |
| `REFRESH_INTERVAL` | `5m` | 重扫间隔 |
| `MIN_PREFIX_LEN` | `4` | SHA 前缀最短长度 |
| `STRICT_SCHEME` | `true` | 只接受 `http`/`https` |
