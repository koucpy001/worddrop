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

## License

MIT — see [LICENSE](LICENSE).
