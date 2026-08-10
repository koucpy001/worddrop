import 'dart:async';
import 'dart:io';

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated_io.dart'
    show ExternalLibrary;
import 'package:flutter_test/flutter_test.dart';

import 'package:app/src/rust/api/events.dart';
import 'package:app/src/rust/api/hello.dart';
import 'package:app/src/rust/frb_generated.dart';

// T16 bridge smoke test: exercises the REAL cdylib built by cargokit.
// Not named *_test.dart on purpose - `flutter test` must stay hermetic
// (no native build required); run explicitly after `flutter build linux`:
//   flutter test test/bridge_smoke.dart
void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  final soPath = 'build/linux/x64/debug/bundle/lib/libmy_croc_bridge.so';

  test('bridge hello() answers through the native cdylib', () async {
    expect(File(soPath).existsSync(), isTrue,
        reason: 'run `flutter build linux --debug` first');
    await RustLib.init(externalLibrary: ExternalLibrary.open(soPath));
    final greeting = await hello(name: 'smoke');
    expect(greeting, contains('my-croc'));
  });

  test('subscribeEvents() yields BridgeEvents', () async {
    final stream = subscribeEvents();
    final events = <BridgeEvent>[];
    final done = Completer<void>();
    // NOTE: never await subscription.cancel() on frb streams - the cancel
    // future never completes in the Dart VM (listenAndBuffer quirk).
    stream.listen((event) {
      events.add(event);
      if (event.kind == 'test') done.complete();
    });
    // Give the Rust-side forwarding task time to subscribe to the event bus
    // (a broadcast send with no receivers is dropped).
    await Future<void>.delayed(const Duration(milliseconds: 500));
    await emitEvent(kind: 'test', message: 'payload');
    await done.future.timeout(const Duration(seconds: 10));
    expect(events, hasLength(1));
    expect(events.first.kind, 'test');
    expect(events.first.message, 'payload');
  });
}
