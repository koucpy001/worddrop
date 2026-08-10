# my-croc

Cross-platform WAN file transfer: Windows GUI, macOS GUI, Linux CLI, Android.

Pair with a short word-code phrase (`nameplate-word-word-word`), transfer files
end-to-end encrypted with resumable progress. Self-host the rendezvous +
relay for stable transfers.

## Status

Workspace scaffolded. Under active development — see `.omo/plans/my-croc.md`
for the work plan and todos.

## Layout

- `crates/core` — pairing (SPAKE2 word-code), session state machine, iroh transfer engine, persistent identity, resume records
- `crates/rendezvous` — axum mailbox server (code <-> ticket, one-shot claim, TTL, rate limits)
- `crates/cli` — Linux CLI (send/receive by word code)
- `flutter/app` — Flutter GUI (Linux desktop + Android), native bridge via flutter_rust_bridge + cargokit

## Install (安装)

### Linux CLI

版本从 `cargo build` 时的 `Cargo.toml` 编译进二进制（`my-croc --version`），无运行时 env 覆盖。

两种方式任选其一：

1. 源码构建（推荐，当前版本）：

   ```sh
   cargo build --release -j 2          # release profile: opt-level=z, lto, strip
   install -m 0755 target/release/my-croc ~/.local/bin/
   ```

2. 下载预编译二进制：仓库推送到 GitHub 后，从 Actions 工件（`my-croc` linux artifact）下载，
   解压后 `chmod +x my-croc` 并放入 `PATH`。本机不提供静态链接（依赖系统 glibc）。

用法：`my-croc send <file...>` / `my-croc receive --code <word-code>`（详见 `--help`）。

### Android（APK / AAB）

Release 包在 `flutter/app/build/app/outputs/` 下：

- APK：`flutter-apk/app-release.apk` — 直接安装：`adb install -r` 或拷贝到手机
  （首次需允许"安装未知来源应用"）。
- AAB：`bundle/app-release.aab` — 用于上架 Google Play 商店（当前未上架，仅本地构建产物）。

签名说明：release 构建暂用 debug 签名密钥（`build.gradle.kts` 中
`signingConfig = signingConfigs.getByName("debug")`）——仅适用于自用安装，
正式分发前需换成正式签名。

### Windows / macOS GUI（CI 构建，未签名）

本开发机为 Linux 主机，无法本地构建 Windows/macOS 目标——`my-croc.exe` 与
macOS `.app` 由 GitHub Actions（`.github/workflows/ci.yml`）在仓库推送后自动构建并
作为 CI 工件提供。两个平台均为**未签名**构建，安装时会有系统警告：

- **Windows**：SmartScreen 会提示"未知发布者"。点击"更多信息" → "仍要运行"即可
  （Microsoft Defender 无法验证发布者，因为二进制未签名）。
- **macOS**：Gatekeeper 会阻止直接打开（未签名 + 未公证）。右键点击 `.app` →
  **打开**（Open）→ 再次确认即可。注意：公证（notarization）需要付费 Apple
  Developer 账号，本项目明确不做（out of scope）。

## Android 设备测试清单 (deferred — needs a physical device)

T20/T21 的验收只到 `flutter build apk --debug` + `flutter test`（构建机无
emulator/KVM，无法真机验证）。在有实体 Android 设备时按以下清单执行并把结果
记录到 `.omo/evidence/`（目前标记为 deferred，不代表已通过）：

1. 安装调试包：`flutter build apk --debug` 产物在
   `flutter/app/build/app/outputs/flutter-apk/app-debug.apk`，`adb install -r` 或
   拷贝到手机安装（首次安装需允许"安装未知来源应用"）。
2. 授予权限：设置 → 应用 → my-croc → 权限 → 允许"存储/文件"（传文件用）与
   "通知"（进度提示）。
3. 传输前先启动自托管服务：本机启动 `iroh-relay` 与 `my-croc-rendezvous`（T6 产物），
   并在 GUI 设置页填入服务地址。
4. 注意：设备上的服务地址**不能写 `127.0.0.1`**（那是设备自己）——必须填写
   宿主机的局域网 IP（如 `http://192.168.x.x:8080` / `http://192.168.x.x:3340`，
   emulator 专用的 `10.0.2.2` 不适用于真机）。宿主防火墙需放行对应端口。
5. 桌面 → Android 传输：两端配对同一 word code，验证文件到达、内容一致
   （对比 sha256）。
6. 中断续传：传输进行中杀掉 app（或关飞行模式），重新打开后应提示
   "继续上次传输"并可续传完成。
7. 反向：Android → 桌面同样验证一次。

## 自托管部署与 TLS (Self-hosted deployment & TLS story)

部署产物在 [`deploy/`](deploy/)：`docker-compose.yml`（iroh-relay + my-croc-rendezvous
两个服务，`restart: unless-stopped` + healthcheck）、systemd unit 文件、生产配置模板与
详细 VPS 部署文档。客户端只需两个环境变量（或 `my-croc config set`）指向自托管服务：

```sh
export MY_CROC_RENDEZVOUS_URL=http://<host>:8080
export MY_CROC_RELAY_URL=http://<host>:3340
```

### TLS 两条路径

**生产 (a)：域名 + Let's Encrypt。** iroh-relay 1.0.3 的 TLS 不是命令行参数
（没有 `--tls-cert/--tls-key`），而是写在 TOML 配置里（`--config-path` 传入）：

```toml
[tls]
cert_mode = "LetsEncrypt"     # 或 "Manual"（自备公开 CA 证书）
hostname = ["relay.example.com"]
contact  = "admin@example.com"
cert_dir = "/var/lib/iroh-relay"
```

iroh-relay 自带 ACME 流程自动申请/续期（LetsEncrypt 模式），提供 HTTPS 443 +
QUIC UDP 7842。客户端侧把 relay 地址指到 `https://relay.example.com` 即可——
iroh 客户端用内置 Mozilla webpki roots 校验，证书必须来自公开 CA。
rendezvous 是普通 HTTP 服务，生产用 Caddy/nginx 做 HTTPS 反代（自动证书），
8080 只监听 127.0.0.1。

**开发 (b)：本地无 TLS / 自签名。** 本地开发用 `--dev` 纯 HTTP 模式（端口 3340，
不需要任何证书）——这是本仓库所有本地测试使用的方式。自签名 HTTPS relay
**无法**被未改动的 my-croc 客户端信任：iroh 1.0.3 的 `Endpoint::builder()` 只暴露
`RelayMode::Custom(url)`，relay 连接用内置 webpki roots 校验（`platform-verifier`
feature 未启用，装进系统信任库无效），也没有"关闭证书校验"的开关。想验证
TLS 全链路只能配真实域名 + Let's Encrypt。详见 [`deploy/README.md`](deploy/README.md)。

## License

MIT — see [LICENSE](LICENSE).
