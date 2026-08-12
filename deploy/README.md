# my-croc 自托管部署 (Self-hosted deployment)

部署 my-croc 的配对服务（rendezvous）与传输中继（iroh-relay）。两套方案任选：

| 方案 | 适合 | 文件 |
| :--- | :--- | :--- |
| Docker Compose | 一台 VPS 快速起两个容器 | `docker-compose.yml` |
| systemd | 裸机 / 不想用 Docker | `iroh-relay.service` + `my-croc-rendezvous.service` |

两个服务：

- **my-croc-rendezvous** — 配对信箱：`code <-> ticket`，axum HTTP 服务，`/health` 探活。
- **iroh-relay 1.0.3** — iroh 传输中继（QUIC + HTTP/WS 代理），客户端打洞失败时走它。

> 部署产物只包含配置模板，**不包含任何 TLS 证书/私钥**（证书请在你的 VPS 上生成）。

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
docker exec my-croc-caddy ls /data/caddy/certificates/   # 两个域名的 LE 证书已持久化
```

重启策略 `restart: unless-stopped`：容器崩溃/机器重启后自动拉起。

客户端指向生产服务（两端都要设置）：

```sh
export MY_CROC_RENDEZVOUS_URL=https://pair.worddrop.cloud
export MY_CROC_RELAY_URL=https://relay.worddrop.cloud
```

LAN/dev 客户端指向宿主机：

```sh
export MY_CROC_RENDEZVOUS_URL=http://<服务器IP>:8080
export MY_CROC_RELAY_URL=http://<服务器IP>:3340        # iroh-relay-dev（--dev 模式）
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
sudo install -m 0755 my-croc-rendezvous /usr/local/bin/
sudo install -m 0755 iroh-relay /usr/local/bin/

# 创建服务用户
sudo useradd --system --home /var/lib/my-croc --shell /usr/sbin/nologin my-croc
sudo useradd --system --home /var/lib/iroh-relay --shell /usr/sbin/nologin iroh-relay

# 安装 unit 文件 + relay 生产配置
sudo install -m 0644 my-croc-rendezvous.service /etc/systemd/system/
sudo install -m 0644 iroh-relay.service /etc/systemd/system/
sudo install -d -o iroh-relay -g iroh-relay /etc/iroh-relay
sudo install -m 0640 -o iroh-relay -g iroh-relay iroh-relay.prod.toml /etc/iroh-relay/relay.toml

sudo systemctl daemon-reload
sudo systemctl enable --now my-croc-rendezvous iroh-relay
systemctl status my-croc-rendezvous iroh-relay
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
| 3340 | TCP | iroh-relay | dev 模式 HTTP relay；生产模式可不开 |
| 80 / 443 | TCP | iroh-relay | 生产 HTTPS relay（443）；80 仅 Let's Encrypt 需要 |
| 7842 | UDP | iroh-relay | 生产 QUIC relay（NAT 打洞地址发现） |
| 8080 | TCP | rendezvous | 生产必须放在反代后，只对反代开放（或 127.0.0.1） |
| 9090 | TCP | iroh-relay | metrics（可选，默认开） |

```sh
# ufw 示例（生产）
sudo ufw allow 80/tcp 443/tcp 7842/udp
sudo ufw allow 8080/tcp comment 'my-croc rendezvous (behind proxy in prod)'
```

> 生产建议：**不要**把 8080 直接暴露公网。用 Caddy/nginx 反代成 HTTPS（§4），
> 8080 只监听 127.0.0.1。

---

## 4. TLS 说明 (TLS story)

### 为什么需要 TLS

- **iroh-relay**：生产模式提供 HTTPS（443）与 QUIC（UDP 7842）两个入口。
  iroh 客户端（my-croc 使用）把 relay 当成可信传输路径，**relay 连接默认走
  QUIC-over-TLS**；没有有效证书，客户端无法验证 relay 身份。dev 模式
  （`--dev`）是纯 HTTP，仅限本机/内网调试。
- **my-croc-rendezvous**：配对信箱。生产必须 HTTPS（防止中间人篡改 code/ticket），
  由反代（Caddy/nginx）终结 TLS。

### 生产路径 (a)：域名 + Let's Encrypt

iroh-relay 1.0.3 **没有 `--tls-cert/--tls-key` 命令行参数**——TLS 配置在
TOML 配置文件里（`--config-path` 传入）：

```toml
# /etc/iroh-relay/relay.toml
[tls]
cert_mode = "LetsEncrypt"     # 或 "Manual"
hostname = ["relay.example.com"]
contact  = "admin@example.com"
prod_tls = true
cert_dir = "/var/lib/iroh-relay"
```

- **LetsEncrypt 模式**：iroh-relay 自带 ACME 流程，自动申请/续期，证书缓存写在
  `cert_dir`。
- **Manual 模式**：用 `certbot certonly --standalone`（或任何公开 CA）拿到证书后，
  填 `manual_cert_path` / `manual_key_path`（默认 `<cert_dir>/default.crt|.key`）。

客户端（CLI / GUI）只需把 relay 地址指到 HTTPS 域名：

```sh
export MY_CROC_RELAY_URL=https://relay.example.com
export MY_CROC_RENDEZVOUS_URL=https://rendezvous.example.com
```

iroh 客户端用内置的 Mozilla webpki roots 验证证书——**证书必须来自公开 CA**。

rendezvous 的 HTTPS 用 Caddy 一行搞定（自动 HTTPS + 反代到 8080）：

```caddyfile
rendezvous.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

### 开发路径 (b)：本地自签名/无 TLS 的真相

1. **最简单（推荐，本项目全程使用）**：relay 用 `--dev` 跑纯 HTTP——
   客户端 `MY_CROC_RELAY_URL=http://<host>:3340`，**不需要任何证书**。
   这就是 compose 默认配置和本地 e2e 用的方式。

2. **自签名证书 + 客户端信任**：iroh 1.0.3 **没有**"关闭证书校验"或"信任自定义 CA"
   的客户端开关——`Endpoint::builder()` 只暴露 `RelayMode::Custom(url)`，relay
   连接用内置 webpki roots 校验（默认 features 不含 `platform-verifier`，装进
   系统信任库也没用）。因此**自签名 HTTPS relay 无法被未改动的 my-croc 客户端信任**。

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
export MY_CROC_RENDEZVOUS_URL=https://pair.worddrop.cloud
export MY_CROC_RELAY_URL=https://relay.worddrop.cloud

# 方式二：配置文件（config.toml；env > file > default）
my-croc config set rendezvous_url https://pair.worddrop.cloud
my-croc config set relay_url https://relay.worddrop.cloud
my-croc config get
```

开发 / 局域网（LAN dev 路径）：`http://<服务器IP>:8080`（rendezvous）+
`http://<服务器IP>:3340`（iroh-relay-dev，`--dev` 模式），见 §1 的 LAN/dev 示例。

| 配置项 | 环境变量 | 配置文件（`my-croc config set`） |
| :--- | :--- | :--- |
| rendezvous | `MY_CROC_RENDEZVOUS_URL` | `rendezvous_url` |
| relay | `MY_CROC_RELAY_URL` | `relay_url` |

GUI：设置页填写（与 CLI 同一份配置）。

---

## 5. 运维提示

- 版本固定：iroh-relay **1.0.3**（客户端 iroh 1.0.3 与之配套）；升级需两端同步。
- 证书续期：LetsEncrypt 模式自动；Manual 模式需自行 cron（`certbot renew` +
  `systemctl restart iroh-relay`）。
- relay 流量是明文转发（iroh 传输层 E2E 加密），relay 本身看不到文件内容。
- 磁盘：relay 日志限 10MB×3（compose）/ journald（systemd）。
