// T17 bridge smoke test: exercises the REAL cdylib end to end over Dart —
// session creation, event streaming, config, a full sender/receiver pair, and
// cancel propagation from Dart to the Rust cancel watch.
//
// Not named *_test.dart on purpose — `flutter test` must stay hermetic (no
// native build required); run explicitly after building the bridge cdylib:
//   cargo build -p my_croc_bridge   (with CARGO_TARGET_DIR pointing at the
//                                    shared root target — see task-17 evidence)
//   MY_CROC_BRIDGE_SO=$PWD/../../target/debug/libmy_croc_bridge.so \
//     flutter test test/bridge_smoke.dart
// The harness script must set MY_CROC_DATA_DIR / MY_CROC_CONFIG_DIR to fresh
// temp dirs and MY_CROC_SMOKE_RV_URL to a running local rendezvous server.
// The config test below then writes rendezvous_url + relay_url through the
// bridge's setConfig, which the session tests rely on (config resolution is
// env > file > default, so the harness only sets the two dirs).

import 'dart:async';
import 'dart:io';

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated_io.dart'
    show ExternalLibrary;
import 'package:flutter_test/flutter_test.dart';

import 'package:app/src/rust/api/config.dart';
import 'package:app/src/rust/api/events.dart';
import 'package:app/src/rust/api/hello.dart';
import 'package:app/src/rust/api/session.dart';
import 'package:app/src/rust/frb_generated.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  late final String soPath;

  setUpAll(() async {
    soPath = Platform.environment['MY_CROC_BRIDGE_SO'] ??
        'build/linux/x64/debug/bundle/lib/libmy_croc_bridge.so';
    expect(File(soPath).existsSync(), isTrue,
        reason: 'build the bridge cdylib first (see header comment)');
    await RustLib.init(externalLibrary: ExternalLibrary.open(soPath));
  });

  test('hello() answers through the native cdylib', () async {
    final greeting = await hello(name: 'smoke');
    expect(greeting, contains('my-croc'));
  });

  test('subscribeEvents() yields BridgeEvents (T16 debug bus)', () async {
    final stream = subscribeEvents();
    final events = <BridgeEvent>[];
    final done = Completer<void>();
    // NOTE: never await subscription.cancel() on frb streams — the cancel
    // future never completes in the Dart VM (listenAndBuffer quirk).
    stream.listen((event) {
      events.add(event);
      if (event.kind == 'test') done.complete();
    });
    await Future<void>.delayed(const Duration(milliseconds: 500));
    await emitEvent(kind: 'test', message: 'payload');
    await done.future.timeout(const Duration(seconds: 10));
    expect(events, hasLength(1));
    expect(events.first.kind, 'test');
    expect(events.first.message, 'payload');
  });

  // Runs first on purpose: the pair tests below create sessions that read
  // these config-file values (env only sets the two dirs).
  test('config get/set round-trips through the bridge', () async {
    final rvUrl = Platform.environment['MY_CROC_SMOKE_RV_URL'] ??
        (throw StateError('MY_CROC_SMOKE_RV_URL is required'));
    final stored = await setConfig(key: 'rendezvous_url', value: rvUrl);
    expect(stored, rvUrl);
    expect(await setConfig(key: 'relay_url', value: 'disabled'), 'disabled');

    final after = await getConfig();
    expect(after.rendezvousUrl, rvUrl);
    expect(after.relayUrl, 'disabled');
    // Env wins over the file: MY_CROC_DATA_DIR (harness) beats any file value.
    expect(after.dataDir, Platform.environment['MY_CROC_DATA_DIR']);

    await expectLater(setConfig(key: 'bogus', value: 'x'), throwsA(anything));
  });

  test('happy path: Dart-created sender+receiver pair transfers a file', () async {
    final tmp = await Directory.systemTemp.createTemp('my-croc-smoke-happy');
    final content = List<int>.generate(4096, (i) => (i * 7) % 251);
    final src = File('${tmp.path}/smoke.txt')..writeAsBytesSync(content);
    final outDir = '${tmp.path}/out';

    final sender = await createSession(role: SessionRole.sender);
    final receiver = await createSession(role: SessionRole.receiver);
    final senderEvents = <BridgeEvent>[];
    final receiverEvents = <BridgeEvent>[];
    final senderDone = Completer<void>();
    final receiverDone = Completer<void>();
    watchTransfer(handle: sender).listen((event) {
      senderEvents.add(event);
      if (event.kind == 'done') senderDone.complete();
    });
    watchTransfer(handle: receiver).listen((event) {
      receiverEvents.add(event);
      if (event.kind == 'done') receiverDone.complete();
    });
    // Give the Rust forwarder tasks time to subscribe to the session buses.
    await Future<void>.delayed(const Duration(milliseconds: 300));

    final prepared = await sendPaths(handle: sender, paths: [src.path]);
    expect(prepared.code, contains('-'), reason: 'code is nameplate-word-word-word');
    expect(prepared.files, hasLength(1));
    expect(prepared.files.first.name, 'smoke.txt');
    expect(prepared.totalBytes.toInt(), content.length);
    expect(await sessionPhase(handle: sender), 'pending_pair');

    final offer = await receiveTicket(handle: receiver, code: prepared.code);
    expect(offer.files, hasLength(1));
    expect(offer.totalBytes.toInt(), content.length);
    expect(await sessionPhase(handle: receiver), 'paired');

    await acceptOffer(handle: receiver, targetDir: outDir);
    await receiverDone.future.timeout(const Duration(seconds: 30));
    await senderDone.future.timeout(const Duration(seconds: 30));

    expect(await sessionPhase(handle: receiver), 'done');
    expect(await sessionPhase(handle: sender), 'done');
    final landed = File('$outDir/smoke.txt').readAsBytesSync();
    expect(landed, equals(content));
    // The event streams carried progress.
    expect(receiverEvents.any((event) => event.kind == 'downloading'), isTrue,
        reason: 'receiver stream should carry downloading progress');
    expect(senderEvents.any((event) => event.kind == 'served' || event.kind == 'done'),
        isTrue,
        reason: 'sender stream should carry served progress or done');

    await disposeSession(handle: receiver);
    await disposeSession(handle: sender);
  });

  test('failure path: cancel from Dart propagates to the Rust cancel watch',
      () async {
    final tmp = await Directory.systemTemp.createTemp('my-croc-smoke-cancel');
    final src = File('${tmp.path}/cancel.txt')..writeAsStringSync('cancellable');

    final sender = await createSession(role: SessionRole.sender);
    final receiver = await createSession(role: SessionRole.receiver);
    final receiverCancelled = Completer<void>();
    final senderCancelled = Completer<void>();
    watchTransfer(handle: receiver).listen((event) {
      if (event.kind == 'phase' && event.phase == 'cancelled') {
        receiverCancelled.complete();
      }
    });
    watchTransfer(handle: sender).listen((event) {
      if (event.kind == 'phase' && event.phase == 'cancelled') {
        senderCancelled.complete();
      }
    });
    await Future<void>.delayed(const Duration(milliseconds: 300));

    final prepared = await sendPaths(handle: sender, paths: [src.path]);
    await receiveTicket(handle: receiver, code: prepared.code);

    await cancelSession(handle: receiver);
    // The core session cancel watch fires on the receiver; the peer Cancel
    // travels over the control stream so the sender unwinds too.
    await receiverCancelled.future.timeout(const Duration(seconds: 15));
    await senderCancelled.future.timeout(const Duration(seconds: 15));
    expect(await sessionPhase(handle: receiver), 'cancelled');
    expect(await sessionPhase(handle: sender), 'cancelled');

    await disposeSession(handle: receiver);
    await disposeSession(handle: sender);
  });
}
