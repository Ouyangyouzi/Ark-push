# Ark-push

> **LitePan 的边车服务（Rust 重写版）** · 不依赖 Meilisearch · 单二进制 · 单端口 `7701`
>
> 版本号见 [`Cargo.toml`](./Cargo.toml) 与 GitHub Releases；镜像标签见下方 GHCR 地址。

Ark-push 是 `Arkpan-push` 的 Rust 重写实现：保留 LitePan 边车的搜索、离线推送、STRM、演员资料、备份等工作流，但**移除了 Meilisearch 进程和 `/meili_data` 卷**，全文检索改为进程内嵌的 SQLite FTS5 索引。它只通过 HTTP API 与 LitePan 交互，不修改 LitePan 的镜像、数据库或配置。

## 主要特性

- 单进程 HTTP 服务（工作台、播放器、油猴脚本、插件 SDK 均在 `7701` 端口）
- 认证：PBKDF2-SHA256 + HttpOnly Cookie，支持 `X-Auth-Token` / `X-Ark-Key`
- LitePan 客户端：登录与凭据持久化、账号选择、目录解析、离线任务、STRM 生成、流式播放
- 推送作业与待生成管理（SQLite `pending.db`）
- 内嵌搜索：SQLite FTS5 trigram + 拼音别名，支持 `filter`/`sort`/`highlight`/`facets`
- 文件与目录 BFS 索引、演员资料与目录匹配、兼容旧版加密备份
- CLI：`serve`、`sync`、`link`、`doctor`、`probe`、`push`
- 空载内存约 6–10 MB（旧版约 500 MB）

> 功能以实际代码为准。部分兼容接口为占位实现，详见 [`src/server.rs`](./src/server.rs) 的路由与 handler。

## 快速开始

```bash
cd Ark-push
docker compose up -d          # 默认拉取 GHCR 预构建镜像
# 打开 http://<NAS-IP>:7701，默认账号 admin / admin，登录后请立即修改密码
```

完整安装、配置、备份与排错见 **[INSTALL.md](./INSTALL.md)**。

## 部署

### Docker Compose（推荐）

仓库根目录的 `docker-compose.yml` 默认从 GHCR 拉取预构建镜像，完整模板如下：

```yaml
services:
  ark-push:
    image: ghcr.io/ouyangyouzi/ark-push:latest
    # build: .   # 想本地编译时才取消注释，并把 image 改成 ark-push:local
    container_name: ark-push
    restart: unless-stopped
    ports:
      - "7701:7701"
    environment:
      - TZ=Asia/Shanghai
      - LINK_115=off
      - LITEPAN_URL=https://<你的LitePan域名>:<端口>
      - LITEPAN_INSECURE=on
      - STATE_DIR=/state
      - STRM_ROOT=/src
      - LOG_LEVEL=info
    volumes:
      - ./data/state:/state
```

- 容器通过网络访问 LitePan，把 `LITEPAN_URL` 改成你自己的地址；自签证书需 `LITEPAN_INSECURE=on`。
- 若报 `401 Unauthorized` / `denied`：先 `docker login ghcr.io`，或到仓库 **Packages → ark-push → Package settings → Change visibility → Public**。
- 想换成自己 fork 的镜像，把 `image` 改成 `ghcr.io/<你的用户名>/<仓库名>:latest`。

workflow 自动推送的标签：`latest`、`main`、`sha-<提交哈希>`。

### 其他方式

```bash
# 从源码本地构建：取消 compose 里的 build 注释、image 改 ark-push:local
docker compose up -d --build

# 本地直接运行（需 Rust 1.90+ 与相关环境变量）
cargo run --release -- serve

# 开发模式（热重载，端口 17701）
docker compose -f docker-compose.dev.yml up -d --build
```

## 配置

全部环境变量见 **[.env.example](./.env.example)**，LitePan 地址 / 账号 / 密码也可在 Web「设置 → 连接」页保存（写入 `/state/state.json`）。

常用项：`HTTP_PORT`、`STATE_DIR`、`STRM_ROOT`、`LITEPAN_URL`、`LITEPAN_USERNAME`、`LITEPAN_PASSWORD`、`LITEPAN_INSECURE`、`LINK_115`、`LINK_ENABLED`、`SYNC_INTERVAL_MIN`、`LINK_INTERVAL_MIN`、`OFFLINE_TIMEOUT_MIN`、`NETDISK_MAX_DIRS`、`LOG_LEVEL`、`TZ`。布尔值支持 `1`/`true`/`on`/`yes`/`y`。

数据全部在 `/state`（`state.json`、`pending.db`、`search_fts.db`、`fileindex/`、`creds/`）。索引为派生数据可删除重建；`state.json` 与 `pending.db` 建议备份。

## CLI

```bash
ark-push serve                 # 启动 HTTP 服务（默认）
ark-push sync [--full]         # 一次性索引同步
ark-push link [--dry-run]      # 触发 LitePan STRM 任务
ark-push doctor                # 健康检查
ark-push probe                 # LitePan 能力探测
ark-push push --magnet <URL> [--target <路径>] [--file-name <名称>]
```

容器内可用 `docker compose exec ark-push ark-push doctor` 调用。

## 与旧版的主要差异

搜索后端由 Meilisearch 改为 SQLite FTS5 trigram + BM25；中文用 trigram 子串匹配并写入拼音别名，错字容忍弱于 Meili；改用 HTTP/1.1 keep-alive；索引文件由 Meili 数据目录改为 `/state/search_fts.db`。接口入口保持兼容，排序结果可能与旧版不同。

## 许可

仅限个人学习与自用，跟随上游 LitePan 项目用途。
