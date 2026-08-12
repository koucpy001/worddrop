// Widget tests for the TransferHistory wiring (send/receive screens).
//
// Regression guard for the FIX-2 change: TransferHistory.addActive /
// completeTransfer / failTransfer / cancelTransfer were DEAD CODE (only
// exercised by unit tests) — the screens never registered live transfers, so
// no record ever reached the persisted history. These tests drive the real
// screen event handlers through the fake backend and assert the singleton
// service state transitions (active card registered on start, terminal
// record on done/error/cancel).
//
// Mirrors pairing_screens_test.dart (fake backend + emit) and
// transfers_screen_test.dart (SharedPreferences mock + singleton reset).

import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:app/backend/fake_backend.dart';
import 'package:app/screens/receive_screen.dart';
import 'package:app/screens/send_screen.dart';
import 'package:app/services/transfer_history.dart';
import 'package:app/src/rust/api/events.dart';
import 'package:app/src/rust/api/session.dart'
    show FileMetaDto, PreparedSendDto;
import 'package:flutter/material.dart';

const _code = '7-correct-horse-battery';

PreparedSendDto _sendDto() => PreparedSendDto(
      code: _code,
      files: [
        FileMetaDto(name: 'a.txt', size: BigInt.from(1024), hash: 'h1'),
        FileMetaDto(name: 'b.bin', size: BigInt.from(2048), hash: 'h2'),
      ],
      totalBytes: BigInt.from(3072),
    );

Widget _sendApp(FakePairingBackend backend) => MaterialApp(
      home: SendScreen(
        backendFactory: (_) async => backend,
        pickFiles: () async => ['/tmp/a.txt', '/tmp/b.bin'],
      ),
    );

Widget _receiveApp(FakePairingBackend backend) => MaterialApp(
      home: ReceiveScreen(backendFactory: (_) async => backend),
    );

Future<void> _pumpFrames(WidgetTester tester, [int frames = 3]) async {
  for (var i = 0; i < frames; i++) {
    await tester.pump(const Duration(milliseconds: 100));
  }
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  setUp(() async {
    SharedPreferences.setMockInitialValues({});
    TransferHistory.resetForTest();
  });

  tearDown(() async {
    await TransferHistory.clearForTest();
  });

  group('SendScreen history wiring', () {
    testWidgets(
        'served registers an active sent transfer; done persists a completed record',
        (tester) async {
      final backend = FakePairingBackend(prepareResult: _sendDto());
      await tester.pumpWidget(_sendApp(backend));
      await tester.tap(find.text('选择文件'));
      await _pumpFrames(tester);

      // No active card before the transfer starts.
      expect(TransferHistory.instance.hasActive, isFalse);

      backend.emit(BridgeEvent(
          kind: 'served',
          received: BigInt.from(1536),
          total: BigInt.from(3072)));
      await tester.pump();

      // Active card registered with the send details.
      final active = TransferHistory.instance.active;
      expect(active, hasLength(1));
      expect(active.single.code, _code);
      expect(active.single.names, ['a.txt', 'b.bin']);
      expect(active.single.totalBytes, BigInt.from(3072));
      expect(active.single.direction, 'sent');
      expect(active.single.progress, closeTo(0.5, 0.001));

      backend.emit(const BridgeEvent(kind: 'done'));
      await tester.pump();

      // Terminal: moved to persisted history as completed (nameplate only —
      // the word-code is never persisted).
      expect(TransferHistory.instance.hasActive, isFalse);
      final history = TransferHistory.instance.history;
      expect(history, hasLength(1));
      expect(history.single.status, 'completed');
      expect(history.single.direction, 'sent');
      expect(history.single.nameplate, '7');
      expect(history.single.names, ['a.txt', 'b.bin']);
      expect(history.single.bytes, BigInt.from(3072));
    });

    testWidgets('error during transfer persists a failed record',
        (tester) async {
      final backend = FakePairingBackend(prepareResult: _sendDto());
      await tester.pumpWidget(_sendApp(backend));
      await tester.tap(find.text('选择文件'));
      await _pumpFrames(tester);

      backend.emit(BridgeEvent(
          kind: 'served',
          received: BigInt.zero,
          total: BigInt.from(3072)));
      await tester.pump();
      expect(TransferHistory.instance.hasActive, isTrue);

      backend.emit(const BridgeEvent(kind: 'error', message: '连接中断'));
      await tester.pump();

      expect(TransferHistory.instance.hasActive, isFalse);
      final history = TransferHistory.instance.history;
      expect(history, hasLength(1));
      expect(history.single.status, 'failed');
      expect(history.single.direction, 'sent');
      expect(history.single.nameplate, '7');
    });

    testWidgets('cancelling from the screen persists a cancelled record',
        (tester) async {
      final backend = FakePairingBackend(prepareResult: _sendDto());
      await tester.pumpWidget(_sendApp(backend));
      await tester.tap(find.text('选择文件'));
      await _pumpFrames(tester);

      backend.emit(BridgeEvent(
          kind: 'served',
          received: BigInt.zero,
          total: BigInt.from(3072)));
      await tester.pump();
      expect(TransferHistory.instance.hasActive, isTrue);

      await tester.tap(find.text('取消发送'));
      await tester.pump();

      expect(backend.cancelCalled, isTrue);
      expect(TransferHistory.instance.hasActive, isFalse);
      final history = TransferHistory.instance.history;
      expect(history, hasLength(1));
      expect(history.single.status, 'cancelled');
      expect(history.single.direction, 'sent');
    });
  });

  group('ReceiveScreen history wiring', () {
    testWidgets(
        'accept registers an active received transfer; done persists a completed record',
        (tester) async {
      final backend =
          FakePairingBackend(claimResult: FakePairingBackend.demoOffer());
      await tester.pumpWidget(_receiveApp(backend));

      await tester.enterText(find.byType(TextField), _code);
      await tester.tap(find.text('连接'));
      await tester.pumpAndSettle();

      await tester.tap(find.text('接收'));
      await tester.pump();

      // Active card registered with the receive details (demo offer).
      final active = TransferHistory.instance.active;
      expect(active, hasLength(1));
      expect(active.single.code, _code);
      expect(active.single.names, ['demo-photos.zip', 'notes.txt']);
      expect(active.single.totalBytes, BigInt.from(6144));
      expect(active.single.direction, 'received');

      backend.emit(const BridgeEvent(kind: 'done'));
      await tester.pump();

      expect(TransferHistory.instance.hasActive, isFalse);
      final history = TransferHistory.instance.history;
      expect(history, hasLength(1));
      expect(history.single.status, 'completed');
      expect(history.single.direction, 'received');
      expect(history.single.nameplate, '7');
      expect(history.single.names, ['demo-photos.zip', 'notes.txt']);

      // Drain the fake backend's scripted timers so the test ends clean.
      await tester.pump(const Duration(seconds: 4));
    });

    testWidgets('error during download persists a failed record',
        (tester) async {
      final backend =
          FakePairingBackend(claimResult: FakePairingBackend.demoOffer());
      await tester.pumpWidget(_receiveApp(backend));

      await tester.enterText(find.byType(TextField), _code);
      await tester.tap(find.text('连接'));
      await tester.pumpAndSettle();

      await tester.tap(find.text('接收'));
      await tester.pump();
      expect(TransferHistory.instance.hasActive, isTrue);

      backend.emit(const BridgeEvent(kind: 'error', message: '对端断开'));
      await tester.pump();

      expect(TransferHistory.instance.hasActive, isFalse);
      final history = TransferHistory.instance.history;
      expect(history, hasLength(1));
      expect(history.single.status, 'failed');
      expect(history.single.direction, 'received');
      expect(history.single.nameplate, '7');

      // Drain the fake backend's scripted timers so the test ends clean.
      await tester.pump(const Duration(seconds: 4));
    });
  });
}
