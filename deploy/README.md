# worddrop 自托管部署 (Self-hosted deployment)

部署 worddrop 的配对服务（rendezvous）与传输中继（iroh-relay）。两套方案任选：

| 方案 | 适合 | 文件 |
| :--- | :--- | :--- |
| Docker Compose | 一台 VPS 快速起两个容器 | `docker-compose.yml` |
| systemd | 裸机 / 不想用 Docker | `iroh-relay.service` + `worddrop-rendezvous.service` |

两个服务：

- **worddrop-rendezvous** — 配对信箱：`code <-> ticket`，axum HTTP 服务，`/health` 探活。
- **iroh-relay 1.0.3** — iroh 传输中继（HTTP/WS 代理；QUIC 入口按设计关闭），客户端打洞失败时走它。

> 部署产物只包含配置模板，**不包含任何 TLS 证书/私钥**——生产证书由 Caddy 首次启动时
> 自动向 Let's Encrypt 申请并持久化在 `caddy-data` 卷（见 §4），无需人工生成。

---

## 1. Docker Compose 部署

```sh
# 在 deploy/ 目录
cd deploy
docker compose up -d --build
```

默认是 **生产模式**：relay 以 `--config-path /etc/iroh-relay/relay.toml` 跑纯 HTTP
（容器内 :80，完整协议含 /relay WebSocket 与 "Iroh Relay" 页面，**无 [tls] 段**）；
Caddy 独占宿主机 80/443 TCP，以**自己的** Let's Encrypt 证书终结两个域名的 TLS
（证书持久化在 `caddy-data` 卷，重建容器不丢），并把 `pair.worddrop.cloud` 反代到
rendezvous :8080、`relay.worddrop.cloud`（含 WebSocket）反代到 relay :80。
`iroh-relay-dev` 服务保留 LAN dev 路径（`--dev`，宿主机 3340）。

**启动前先做宿主机端口预检**（宿主机进程占用 3340/9090 时 compose 会报
"port is already allocated"；这些通常是早期 e2e 测试遗留的 dev relay）：

```sh
ss -tlnp | grep -E ':3340|:9090|:8080'
# 若看到宿主进程（非容器）占用 3340/9090：
kill <pid>          # 或 systemctl stop iroh-relay
ss -tlnp | grep -E ':3340|:9090'   # 确认已释放后再 up
```

启动后验证：

```sh
curl -f http://127.0.0.1:3340/        # LAN dev relay 返回 "Iroh Relay" 页面
curl -f http://127.0.0.1:8080/health  # rendezvous 返回 ok
docker compose ps                      # 四个容器 Up (healthy)：relay/rendezvous/caddy/relay-dev
```

公网生产验证（需要 80/443 已在防火墙放行，见 §3）：

```sh
curl -fsSL https://relay.worddrop.cloud/          # "Iroh Relay" 页面（Caddy 终结 TLS）
curl -fsSL https://pair.worddrop.cloud/health     # ok
docker exec worddrop-caddy ls /data/caddy/certificates/   # 两个域名的 LE 证书已持久化
```

> **本路径已实测（2026-08-12，T1/T4）**：https://relay.worddrop.cloud 返回 "Iroh Relay"
> 页面、https://pair.worddrop.cloud/health 返回 ok、LE 证书持久化验证通过；同主机
> CLI↔CLI 经公网 TLS 传输字节一致（同机回环说明见
> `.omo/evidence/my-croc-prod/task-4-my-croc-prod.txt`）。

重启策略 `restart: unless-stopped`：容器崩溃/机器重启后自动拉起。

客户端指向生产服务（两端都要设置）：

```sh
export WORDDROP_RENDEZVOUS_URL=https://pair.worddrop.cloud
export WORDDROP_RELAY_URL=https://relay.worddrop.cloud
```

LAN/dev 客户端指向宿主机：

```sh
export WORDDROP_RENDEZVOUS_URL=http://<服务器IP>:8080
export WORDDROP_RELAY_URL=http://<服务器IP>:3340        # iroh-relay-dev（--dev 模式）
```

### 生产模式说明（Compose 即生产）

`deploy/iroh-relay/relay.toml` 随仓库提供并只读挂载到 `/etc/iroh-relay`——**只有
`http_bind_addr = "0.0.0.0:80"`，没有 [tls] 段、没有证书目录**。TLS 全部由 Caddy 终结：

- relay 以纯 HTTP 跑完整协议（含 /relay WebSocket 与 "Iroh Relay" 页面）在 :80；
  Caddy 把 `relay.worddrop.cloud`（含 WebSocket /relay）反代到 `iroh-relay:80`。
- 为什么 relay 不自带 TLS：iroh-relay 1.0.3 的 [tls] 段会让 :80 变成只返回 404 的
  captive portal，完整协议移到内部 :443；而它内置的 ACME（tokio-rustls-acme 0.9.1）
  只支持 TLS-ALPN-01（无 HTTP-01），在 Caddy 独占 443 时无法完成签发（详见 §4）。
- 无 UDP 端口：QUIC 地址发现（UDP 7842）需要 relay TLS，按设计关闭。
- 证书持久化：LE 证书存在 `caddy-data` 卷（重建容器不丢——修复了旧方案的证书丢失问题）。

---

## 2. systemd 部署（裸机）

```sh
# 安装二进制（先在本机构建或从 CI 工件下载）
sudo install -m 0755 worddrop-rendezvous /usr/local/bin/
sudo install -m 0755 iroh-relay /usr/local/bin/

# 创建服务用户
sudo useradd --system --home /var/lib/worddrop --shell /usr/sbin/nologin worddrop
sudo useradd --system --home /var/lib/iroh-relay --shell /usr/sbin/nologin iroh-relay

# 安装 unit 文件 + relay 生产配置
sudo install -m 0644 worddrop-rendezvous.service /etc/systemd/system/
sudo install -m 0644 iroh-relay.service /etc/systemd/system/
sudo install -d -o iroh-relay -g iroh-relay /etc/iroh-relay
sudo install -m 0640 -o iroh-relay -g iroh-relay iroh-relay.prod.toml /etc/iroh-relay/relay.toml

sudo systemctl daemon-reload
sudo systemctl enable --now worddrop-rendezvous iroh-relay
systemctl status worddrop-rendezvous iroh-relay
```

两个 unit 均为 `Restart=always` + 最小权限硬化（`ProtectSystem=strict`、
`PrivateTmp`、专用用户）。`iroh-relay.service` 带 `AmbientCapabilities=CAP_NET_BIND_SERVICE`
（以非 root 的 iroh-relay 用户绑定 :80），配置文件 `iroh-relay.prod.toml` 是纯 HTTP
模板（只有 `http_bind_addr = "0.0.0.0:80"`，无 [tls]、无证书目录）——裸机路径同样
由 Caddy 在前端终结 TLS。日志：`journalctl -u iroh-relay -f`。

---

## 3. 防火墙端口 (firewall)

| 端口 | 协议 | 服务 | 说明 |
| :--- | :--- | :--- | :--- |
| 80 / 443 | TCP | Caddy | **生产唯一需要放行**；Caddy 独占这两个端口终结两个域名的 TLS（80 用于 LE HTTP-01 签发，443 服务客户端的 wss/HTTPS） |
| 3340 | TCP | iroh-relay-dev | 仅 LAN dev 路径（`--dev`），不对外网开放 |
| 8080 | TCP | worddrop-rendezvous | 仅 LAN dev 路径；生产由 Caddy 反代，不直接暴露 |
| 9090 | TCP | iroh-relay | 可选 dev metrics（默认发布到宿主机），不对外网开放 |

**没有任何 UDP 端口**：QUIC 地址发现（UDP 7842）需要 relay 侧 TLS，按设计关闭——
客户端的 relay 数据路径是 WebSocket-over-TLS（TCP 443），不是 QUIC/UDP。

```sh
# ufw 示例（生产）
sudo ufw allow 80/tcp 443/tcp
```

> 生产建议：**不要**把 8080/3340/9090 直接暴露公网（仅内网/LAN 使用）。

---

## 4. TLS 说明 (TLS story)

### 为什么需要 TLS

- **worddrop-rendezvous**：配对信箱。生产必须 HTTPS（防止中间人篡改 code/ticket），
  由 Caddy 反代终结 TLS。
- **iroh-relay**：客户端（worddrop 使用）把 relay 当成可信传输路径，relay 连接走
  WebSocket-over-TLS（wss，客户端填 `https://relay...` 自动转 wss）；没有有效证书，
  客户端无法验证 relay 身份。dev 模式（`--dev`）是纯 HTTP，仅限本机/内网调试。

### 生产路径：Caddy 终结 TLS（本仓库采用，已实测）

relay **不自带 TLS**——`deploy/iroh-relay/relay.toml` 只有
`http_bind_addr = "0.0.0.0:80"`，**没有 [tls] 段**，relay 以纯 HTTP 跑完整协议
（含 /relay WebSocket 与 "Iroh Relay" 页面）在容器内 :80。Caddy 独占宿主机
80/443 TCP：

- Caddy 用**自己的** Let's Encrypt 证书终结 `relay.worddrop.cloud` 与
  `pair.worddrop.cloud` 的 TLS（HTTP-01 走 :80——因为 Caddy 拥有 :80 才能完成
  签发）；证书持久化在 `caddy-data` 卷，重建容器不丢。
- `relay.worddrop.cloud`（含 WebSocket /relay，Caddy 自动处理 WS 升级）→ 反代到
  relay 容器 :80；`pair.worddrop.cloud` → 反代到 `worddrop-rendezvous:8080`。
- 客户端只需把地址指到 HTTPS 域名；iroh 客户端用内置的 Mozilla webpki roots
  验证证书——**证书必须来自公开 CA**（Caddy 的 LE 证书满足）。

```caddyfile
relay.worddrop.cloud {
    reverse_proxy iroh-relay:80          # 含 WebSocket /relay（Caddy 自动升级）
}
pair.worddrop.cloud {
    reverse_proxy worddrop-rendezvous:8080
}
```

**为什么 relay 不能自带 Let's Encrypt（源码级架构事实）**：iroh-relay 1.0.3 配置
`[tls]` 段后，:80 会退化成只返回 404 的 captive portal，完整协议移到内部
https_bind_addr（默认 :443）；而它内置的 ACME（tokio-rustls-acme 0.9.1）只支持
TLS-ALPN-01、**没有 HTTP-01**——在 Caddy 独占 443 的情况下，LE 对 relay 域名的
acme-tls/1 握手会打到 Caddy 而不是 relay，签发永远无法完成。因此「relay 自带 LE +
前端再挂 Caddy」是走不通的组合，正确架构就是上面的 Caddy 全权终结方案。
（本仓库已按此方案在 worddrop.cloud 实测上线；旧文档里的 relay `[tls] LetsEncrypt`
配置模板已废弃，`deploy/iroh-relay.prod.toml` 现在是纯 HTTP 模板。）

### 开发路径 (b)：本地自签名/无 TLS 的真相

1. **最简单（推荐，本项目全程使用）**：relay 用 `--dev` 跑纯 HTTP——
   客户端 `WORDDROP_RELAY_URL=http://<host>:3340`，**不需要任何证书**。
   这就是 compose 默认配置和本地 e2e 用的方式。

2. **自签名证书 + 客户端信任**：iroh 1.0.3 **没有**"关闭证书校验"或"信任自定义 CA"
   的客户端开关——`Endpoint::builder()` 只暴露 `RelayMode::Custom(url)`，relay
   连接用内置 webpki roots 校验（默认 features 不含 `platform-verifier`，装进
   系统信任库也没用）。因此**自签名 HTTPS relay 无法被未改动的 worddrop 客户端信任**。

   结论：本地开发想验证 TLS 全链路，两个现实选项：
   - 用 `--dev` 纯 HTTP（推荐，功能等价，只是没有加密——本机调试可接受）；
   - 或者给本机配一个真实域名 + Let's Encrypt（staging 目录测试 ACME 流程，
     `IROH_RELAY_ACME_URL` 环境变量可指向本地 ACME 如 pebble）。

   文档没有把"自签名 + 客户端信任"写成一个开箱即用的路径，因为 **1.0.3 版本
   不存在这样的客户端开关**——这是基于源码的事实，不是配置疏漏。

### 客户端服务地址配置

生产（公网 TLS，worddrop.cloud；两端都要设置，CLI 与 GUI 通用）：

```sh
# 方式一：环境变量
export WORDDROP_RENDEZVOUS_URL=https://pair.worddrop.cloud
export WORDDROP_RELAY_URL=https://relay.worddrop.cloud

# 方式二：配置文件（config.toml；env > file > default）
worddrop config set rendezvous_url https://pair.worddrop.cloud
worddrop config set relay_url https://relay.worddrop.cloud
worddrop config get
```

开发 / 局域网（LAN dev 路径）：`http://<服务器IP>:8080`（rendezvous）+
`http://<服务器IP>:3340`（iroh-relay-dev，`--dev` 模式），见 §1 的 LAN/dev 示例。

| 配置项 | 环境变量 | 配置文件（`worddrop config set`） |
| :--- | :--- | :--- |
| rendezvous | `WORDDROP_RENDEZVOUS_URL` | `rendezvous_url` |
| relay | `WORDDROP_RELAY_URL` | `relay_url` |

GUI：设置页填写（与 CLI 同一份配置）。

---

## 5. 运维提示

- 版本固定：iroh-relay **1.0.3**（客户端 iroh 1.0.3 与之配套）；升级需两端同步。
- 证书续期：Caddy 自动续期（LE），无需人工介入；relay 不带 [tls] 段（见 §4）。
- relay 流量是明文转发（iroh 传输层 E2E 加密），relay 本身看不到文件内容。
- 磁盘：relay 日志限 10MB×3（compose）/ journald（systemd）。
