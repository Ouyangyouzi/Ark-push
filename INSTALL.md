# Ark-push 安装与部署指南

本文覆盖从零部署、配置、备份到排错的完整流程。若只想快速启动，见 [README.md 快速开始](./README.md#快速开始)。

---

## 前置条件

| 方式 | 要求 |
|------|------|
| Docker Compose（推荐） | Docker 20.10+ 且包含 `docker compose` 插件；2 GB 以上可用内存 |
| GHCR 镜像 | 同上，且能访问 `ghcr.io` |
| 源码运行 | Rust 1.90+（`rustup`）、`git`；若需要静态资源，保留仓库中的 `ui/` 目录 |

同时需要有一个**可访问的 LitePan 服务**及其登录账号密码。Ark-push 容器通过网络（HTTP API）连接 LitePan 后端，所以 `LITEPAN_URL` 必须是**从容器内部能访问到的地址**——例如远程 `https://<域名>:<端口>`，或同机容器网络里的服务名。

> Ark-push 通过 HTTP API 访问 LitePan，不会修改 LitePan 的镜像、数据库或配置。两者可以跑在同一台 NAS 上，也可以分机部署。若 LitePan 使用自签证书，需设置 `LITEPAN_INSECURE=on`。

---

## 方式一：Docker Compose 一键部署（推荐）

### 1. 获取代码

```bash
git clone https://github.com/Ouyangyouzi/Ark-push.git
cd Ark-push
```

### 2. 检查 Compose 模板

仓库自带的 `docker-compose.yml` 已经是可直接运行的模板，默认从 GHCR 拉取预构建镜像：

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

关于 `image` 这一行：

- `ghcr.io/ouyangyouzi/ark-push:latest` 是**完整的远程拉取地址**：`ghcr.io` 是仓库域名，`ouyangyouzi/ark-push` 是你的仓库路径，`latest` 是标签。因为模板里没有 `build`，`docker compose up` 会直接拉取它，不会在本机编译。
- 若看到 `ark-push:latest` 这种没有域名和路径的名字，那是**本地镜像标签**，Docker 会去 Docker Hub 找同名官方镜像（找不到就报错）——它不是你的 GHCR 拉取地址。
- 想换成自己 fork 的镜像，把这一行改成 `ghcr.io/<你的用户名>/<仓库名>:latest`。

按实际情况调整：

- **LitePan 是远程服务**：把 `LITEPAN_URL` 改成你自己的地址；自签证书保持 `LITEPAN_INSECURE=on`。
- **LitePan 在同机宿主机上**：改成 `http://host.docker.internal:5211`，并取消 `extra_hosts` 的注释。
- **LitePan 在同一 Docker 网络的另一个容器里**：用容器名或服务名，例如 `http://litepan:5211`。
- **端口冲突**：把 `"7701:7701"` 改成 `"<宿主端口>:7701"`。
- **需要本地 STRM / 海报目录**：取消注释 `./data/strm:/src:ro`，并改成实际路径。

也可以在仓库根目录放一个 `.env` 文件（参考 `.env.example`）来覆盖变量，Compose 会自动读取。

### 3. 启动

```bash
# GHCR 包若为私有，先登录（或把包改为 Public，见方式二）
docker login ghcr.io

docker compose up -d
```

首次会从 GHCR 拉取镜像，通常几秒到几十秒。查看进度：

```bash
docker compose logs -f ark-push
```

### 4. 验证

```bash
curl http://localhost:7701/health
# {"ok":true,"service":"ark-push"}

docker compose ps          # 状态应为 Up，健康检查为 healthy
docker compose exec ark-push ark-push doctor
```

浏览器打开 `http://<NAS-IP>:7701`，使用默认账号 `admin` / `admin` 登录。

### 5. 首次登录后必做

1. **修改密码**：「设置 → 账号与安全」。默认密码仅用于首次登录。
2. **填写 LitePan 连接**：「设置 → 连接」填入 LitePan URL、账号、密码并保存；也可通过环境变量配置。
3. **配置索引根目录**：在设置页填写需要索引的网盘根目录（`roots`）。
4. **触发首次索引**：「设置 → 同步」或 `docker compose exec ark-push ark-push sync --full`。

---

## 方式二：从源码本地构建（Compose）

若不想用 GHCR 镜像，可以自己编译。修改 `docker-compose.yml`：

```yaml
image: ark-push:local   # 本地镜像名，Docker 不会去远程拉这个名字
build: .                # 取消这行注释，让 Compose 从 Dockerfile 构建
```

```bash
docker compose up -d --build
```

首次会下载 Rust 基础镜像并编译依赖，通常需要几分钟。此方式不需要登录 GHCR。

### GHCR 私有包处理

方式一默认从 GHCR 拉取。GHCR 包默认是私有的，两种解决办法任选其一：

- **匿名拉取**：进入 GitHub 仓库 → 右侧 **Packages** → ark-push → **Package settings** → **Change visibility → Public**。
- **保持私有**：拉取前先登录,用有 `read:packages` 权限的 GitHub Token：

```bash
echo "<TOKEN>" | docker login ghcr.io -u Ouyangyouzi --password-stdin
```

### 自己 fork 后构建

fork 仓库后，把 compose 里的 `image` 改成 `ghcr.io/<你的用户名>/<仓库名>:latest`。首次需要在仓库 **Actions** 页点击允许 workflow 运行，并确认 `Settings → Actions → General → Workflow permissions` 中勾选了 `read and write permissions`（用于推送 GHCR）。

---

## 方式三：从源码本地运行

适合开发调试，或在不支持 Docker 的环境中使用。

### 1. 安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustc --version    # 需 1.90+
```

Windows 请从 [rustup.rs](https://rustup.rs/) 下载安装器。

### 2. 构建

```bash
git clone https://github.com/Ouyangyouzi/Ark-push.git
cd Ark-push
cargo build --release
```

产物位于 `target/release/ark-push`。

### 3. 配置并运行

```bash
export STATE_DIR=./data/state
export UI_DIR=./ui
export LITEPAN_URL=http://127.0.0.1:5211
export LITEPAN_USERNAME=admin
export LITEPAN_PASSWORD=<你的密码>
export HTTP_PORT=7701

mkdir -p "$STATE_DIR"
./target/release/ark-push serve
```

Windows PowerShell 中用 `$env:STATE_DIR = "./data/state"` 设置环境变量。

> 本地运行时 `LITEPAN_URL` 指向本机地址，不需要 `host.docker.internal`。

---

## 开发模式（热重载）

```bash
docker compose -f docker-compose.dev.yml up -d --build
# 访问 http://<NAS-IP>:17701
```

特点：

- 整个项目目录挂载到容器 `/app`，修改 `src/*.rs` 后 `cargo watch --poll` 自动重编译重启
- 修改 `ui/*` 后刷新浏览器即生效
- 状态仍保存在 `./data/state`，重建容器不会丢登录与待生成
- 使用 `--poll` 兼容 Windows / NAS 的 bind mount（inotify 不一定能收到宿主变更）

首次启动会安装 `cargo-watch` 并编译依赖，CPU 会短暂升高，编译完成后回落。日常使用请用正式 Compose，不要长期运行 dev 容器。

---

## 配置参考

完整环境变量模板见 `.env.example`。常用项：

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `HTTP_PORT` | `7701` | 监听端口 |
| `STATE_DIR` | `/state` | 状态目录（必须可写） |
| `STRM_ROOT` | `/src` | STRM / 海报根目录 |
| `UI_DIR` | `/app/ui` | 静态资源目录 |
| `LITEPAN_URL` | `https://<你的LitePan域名>:<端口>` | LitePan 后端地址（容器通过网络访问，改成你的实际地址） |
| `LITEPAN_INSECURE` | `off` | 自签证书 HTTPS 设为 `on` |
| `LITEPAN_USERNAME` | `admin` | LitePan 账号 |
| `LITEPAN_PASSWORD` | 空 | LitePan 密码 |
| `LITEPAN_INSECURE` | `off` | 忽略自签证书校验 |
| `LINK_115` | `off` | 115 直链开关 |
| `LINK_ENABLED` | `on` | STRM 定时调度 |
| `PUSH_ALLOW_ANON` | `off` | 允许匿名推送 |
| `SYNC_INTERVAL_MIN` | `10` | 索引同步间隔（分钟） |
| `LINK_INTERVAL_MIN` | `60` | STRM 任务间隔（分钟） |
| `OFFLINE_TIMEOUT_MIN` | `120` | 离线任务超时（分钟） |
| `NETDISK_MAX_DIRS` | `20000` | 网盘 BFS 最大目录数 |
| `LOG_LEVEL` | `info` | `trace` / `debug` / `info` / `warn` / `error` |
| `TZ` | `Asia/Shanghai` | 时区 |

布尔变量接受 `1` / `true` / `on` / `yes` / `y`，其他值视为 false。

### 数据与备份

`/state` 目录内容：

| 文件 | 说明 | 能否删除 |
|------|------|----------|
| `state.json` | 设置、会话、认证信息 | 否（删除会重置登录与配置） |
| `pending.db` | 待生成任务 | 否 |
| `search_fts.db` | FTS5 全文索引 | 可删除，之后重建 |
| `fileindex/*.json` | 文件与目录索引 | 可删除，之后重建 |
| `creds/` | LitePan Cookie 凭据 | 可删除，需重新登录 |
| `cache/`、`avatars/` | 缓存与头像 | 可删除 |

推荐备份方式：

```bash
# 停止容器后打包，避免 SQLite WAL 文件不一致
docker compose down
tar -czf ark-push-state-$(date +%F).tar.gz ./data/state
docker compose up -d
```

也可以在 Web 界面使用「备份 → 导出」，生成兼容旧版格式的加密备份文件。

### 升级

```bash
cd Ark-push
git pull
docker compose up -d --build     # 源码构建方式
# 或
docker compose pull && docker compose up -d   # GHCR 镜像方式
```

升级前建议先备份 `/state`。

---

## 常见问题排查

### 容器反复重启

```bash
docker compose logs --tail=200 ark-push
```

常见原因：`LITEPAN_URL` 不是以 `http://` 或 `https://` 开头（配置会直接校验失败）、`STATE_DIR` 无写权限、端口被占用。

### 健康检查失败

确认容器内可访问 `http://127.0.0.1:7701/health`。若改过 `HTTP_PORT`，Compose 中的健康检查会自动读取该变量。

### LitePan 登录失败

```bash
docker compose exec ark-push ark-push doctor
```

- 检查 `LITEPAN_URL` 是否从容器可达：`docker compose exec ark-push wget -qO- <LITEPAN_URL>/...`
- Docker Desktop 默认提供 `host.docker.internal`；Linux 需要模板中的 `extra_hosts: host.docker.internal:host-gateway`
- 若 LitePan 使用自签 HTTPS 证书，设置 `LITEPAN_INSECURE=on`
- 也可在「设置 → 连接」页直接填写并保存凭据

### 搜索无结果

1. 确认已在设置页填写索引根目录（`roots`）
2. 执行一次 `ark-push sync --full`
3. 查看 `/api/stats` 的 `numberOfDocuments` 是否大于 0
4. 若索引损坏，删除 `data/state/search_fts.db` 后重新同步

### 内存占用高

空载 RSS 约 6–10 MB。若内存增长明显，通常是 `fileindex/<account>.json` 过大。可降低 `NETDISK_MAX_DIRS`（默认 20000）减少遍历量。

### 端口冲突

修改 `docker-compose.yml` 中的端口映射，例如 `"7702:7701"`，然后 `docker compose up -d`。

---

## 卸载

```bash
docker compose down
# 若需彻底清除数据（不可恢复）：
# rm -rf ./data/state
```

卸载前请确认已导出需要的备份。
