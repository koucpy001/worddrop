// Widget tests for the pairing screens (T18) — hermetic: a fake backend
// replaces the native bridge, so `flutter test` needs no cdylib.
//
// Covered states:
//  - send: idle -> pick -> preparing -> waiting (code + copy) -> transferring
//          -> done / error / cancelled
//  - receive: code validation -> claim -> offer dialog 接收/拒绝 -> progress
//          -> done / wrong-words error banner (QA failure scenario)

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:app/backend/fake_backend.dart';
import 'package:app/screens/receive_screen.dart';
import 'package:app/screens/send_screen.dart';
import 'package:app/src/rust/api/events.dart';
import 'package:app/src/rust/api/session.dart'
    show FileMetaDto, PreparedSendDto;

const _code = '7-correct-horse-battery';

PreparedSendDto _sendDto() => PreparedSendDto(
      code: _code,
      files: [
        FileMetaDto(name: 'a.txt', size: BigInt.from(1024), hash: 'h1'),
        FileMetaDto(name: 'b.bin', size: BigInt.from(2048), hash: 'h2'),
      ],
      totalBytes: BigInt.from(3072),
    );

Future<List<String>?> _fakePicker(List<String> paths) async => paths;

Widget _sendApp(FakePairingBackend backend) => MaterialApp(
      home: SendScreen(
        backendFactory: (_) async => backend,
        pickFiles: () => _fakePicker(['/tmp/a.txt', '/tmp/b.bin']),
      ),
    );

Widget _receiveApp(FakePairingBackend backend) => MaterialApp(
      home: ReceiveScreen(backendFactory: (_) async => backend),
    );

/// Pumps a few frames. Unlike pumpAndSettle this tolerates indeterminate
/// spinners (waiting/progress states) that animate forever.
Future<void> _pumpFrames(WidgetTester tester, [int frames = 3]) async {
  for (var i = 0; i < frames; i++) {
    await tester.pump(const Duration(milliseconds: 100));
  }
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  group('SendScreen', () {
    testWidgets('idle shows the pick button without touching the backend',
        (tester) async {
      final backend = FakePairingBackend();
      await tester.pumpWidget(_sendApp(backend));

      expect(find.text('选择要发送的文件'), findsOneWidget);
      expect(find.text('选择文件'), findsOneWidget);
      expect(backend.lastPaths, isNull);
    });

    testWidgets(
        'picking files runs prepareSend and shows code + copy + waiting status',
        (tester) async {
      final backend = FakePairingBackend()
        ..sendCompleter = Completer<PreparedSendDto>();
      await tester.pumpWidget(_sendApp(backend));

      await tester.tap(find.text('选择文件'));
      await tester.pump();

      // Preparing state while the bridge works.
      expect(find.textContaining('正在准备文件'), findsOneWidget);
      expect(backend.lastPaths, ['/tmp/a.txt', '/tmp/b.bin']);

      backend.sendCompleter!.complete(_sendDto());
      await _pumpFrames(tester);

      // Code displayed as nameplate + word chips.
      expect(find.text('7'), findsOneWidget);
      expect(find.text('correct'), findsOneWidget);
      expect(find.text('horse'), findsOneWidget);
      expect(find.text('battery'), findsOneWidget);
      expect(find.text('复制配对码'), findsOneWidget);
      expect(find.text('等待接收方输入...'), findsOneWidget);
      expect(find.text('取消发送'), findsOneWidget);
      expect(find.textContaining('2 个文件'), findsOneWidget);
    });

    testWidgets('copy button puts the code on the clipboard', (tester) async {
      final clipboard = <MethodCall>[];
      tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        (call) async {
          if (call.method == 'Clipboard.setData') clipboard.add(call);
          return null;
        },
      );
      final backend = FakePairingBackend(prepareResult: _sendDto());
      await tester.pumpWidget(_sendApp(backend));

      await tester.tap(find.text('选择文件'));
      await _pumpFrames(tester);
      await tester.tap(find.text('复制配对码'));
      await tester.pump();

      expect(clipboard, hasLength(1));
      expect((clipboard.single.arguments as Map)['text'], _code);
      expect(find.text('已复制配对码'), findsOneWidget);

      // Let the SnackBar auto-dismiss timer expire.
      await tester.pump(const Duration(seconds: 4));
      await tester.pump(const Duration(milliseconds: 500));

      tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
          SystemChannels.platform, null);
    });

    testWidgets('served events drive progress; done event shows banner',
        (tester) async {
      final backend = FakePairingBackend(prepareResult: _sendDto());
      await tester.pumpWidget(_sendApp(backend));
      await tester.tap(find.text('选择文件'));
      await _pumpFrames(tester);

      backend.emit(BridgeEvent(
          kind: 'served',
          received: BigInt.from(1536),
          total: BigInt.from(3072)));
      await tester.pump();

      expect(find.text('正在发送...'), findsOneWidget);
      expect(find.textContaining('1.5 KiB / 3.0 KiB'), findsOneWidget);

      backend.emit(const BridgeEvent(kind: 'done'));
      await tester.pump();

      expect(find.text('传输完成'), findsOneWidget);
    });

    testWidgets('error event surfaces an error banner', (tester) async {
      final backend = FakePairingBackend(prepareResult: _sendDto());
      await tester.pumpWidget(_sendApp(backend));
      await tester.tap(find.text('选择文件'));
      await _pumpFrames(tester);

      backend.emit(const BridgeEvent(kind: 'error', message: '连接中断'));
      await tester.pump();

      expect(find.text('传输失败: 连接中断'), findsOneWidget);
    });

    testWidgets('cancel calls the bridge and shows the cancelled banner',
        (tester) async {
      final backend = FakePairingBackend(prepareResult: _sendDto());
      await tester.pumpWidget(_sendApp(backend));
      await tester.tap(find.text('选择文件'));
      await _pumpFrames(tester);

      await tester.tap(find.text('取消发送'));
      await tester.pump();

      expect(backend.cancelCalled, isTrue);
      expect(find.text('已取消发送'), findsOneWidget);
    });

    testWidgets('prepareSend failure shows an error banner', (tester) async {
      final backend =
          FakePairingBackend(prepareError: StateError('磁盘空间不足'));
      await tester.pumpWidget(_sendApp(backend));

      await tester.tap(find.text('选择文件'));
      await tester.pumpAndSettle();

      expect(find.textContaining('准备发送失败'), findsOneWidget);
    });
  });

  group('ReceiveScreen code validation', () {
    testWidgets('bad formats are rejected client-side, bridge never called',
        (tester) async {
      final backend = FakePairingBackend();
      await tester.pumpWidget(_receiveApp(backend));

      // Case is normalized (see the normalization test below), so uppercase
      // input is deliberately NOT in this list.
      for (final bad in ['abc', '7-correct-horse', '7-correct-horse-battery-extra', '07-correct-horse-battery', '12345-correct-horse-battery', '7-correct horse battery']) {
        await tester.enterText(find.byType(TextField), bad);
        await tester.tap(find.text('连接'));
        await tester.pump();
        expect(find.textContaining('配对码格式不正确'), findsOneWidget,
            reason: 'should reject "$bad"');
      }
      expect(backend.lastCode, isNull);
    });

    testWidgets('valid code is normalized (trim + lowercase) and claimed',
        (tester) async {
      final backend =
          FakePairingBackend(claimResult: FakePairingBackend.demoOffer());
      await tester.pumpWidget(_receiveApp(backend));

      await tester.enterText(
          find.byType(TextField), '  7-CORRECT-Horse-Battery  ');
      await tester.tap(find.text('连接'));
      await tester.pumpAndSettle();

      expect(backend.lastCode, '7-correct-horse-battery');
    });
  });

  group('ReceiveScreen offer flow', () {
    testWidgets('claim result opens the offer dialog with names and sizes',
        (tester) async {
      final backend =
          FakePairingBackend(claimResult: FakePairingBackend.demoOffer());
      await tester.pumpWidget(_receiveApp(backend));

      await tester.enterText(find.byType(TextField), _code);
      await tester.tap(find.text('连接'));
      await tester.pumpAndSettle();

      expect(find.text('接收传输？'), findsOneWidget);
      expect(find.text('demo-photos.zip'), findsOneWidget);
      expect(find.text('notes.txt'), findsOneWidget);
      expect(find.text('2.0 KiB'), findsOneWidget); // demo-photos.zip size
      expect(find.text('4.0 KiB'), findsOneWidget); // notes.txt size
      expect(find.textContaining('2 个文件'), findsOneWidget);
      expect(find.text('接收'), findsOneWidget);
      expect(find.text('拒绝'), findsOneWidget);
    });

    testWidgets('拒绝 declines through the bridge and closes the dialog',
        (tester) async {
      final backend =
          FakePairingBackend(claimResult: FakePairingBackend.demoOffer());
      await tester.pumpWidget(_receiveApp(backend));

      await tester.enterText(find.byType(TextField), _code);
      await tester.tap(find.text('连接'));
      await tester.pumpAndSettle();

      await tester.tap(find.text('拒绝'));
      await tester.pumpAndSettle();

      expect(backend.lastDeclineReason, '用户拒绝');
      expect(find.text('接收传输？'), findsNothing);
      expect(find.text('已拒绝传输'), findsOneWidget);
    });

    testWidgets('接收 accepts through the bridge and transfers to done',
        (tester) async {
      final backend =
          FakePairingBackend(claimResult: FakePairingBackend.demoOffer());
      await tester.pumpWidget(_receiveApp(backend));

      await tester.enterText(find.byType(TextField), _code);
      await tester.tap(find.text('连接'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('接收'));
      await _pumpFrames(tester);

      // Empty targetDir = bridge default (config received/ subdir).
      expect(backend.lastTargetDir, '');
      expect(find.text('正在接收...'), findsOneWidget);

      backend.emit(BridgeEvent(
          kind: 'downloading',
          received: BigInt.from(2048),
          total: BigInt.from(6144)));
      await tester.pump();
      expect(find.textContaining('2.0 KiB / 6.0 KiB'), findsOneWidget);

      backend.emit(const BridgeEvent(kind: 'exporting', name: 'notes.txt'));
      await tester.pump();
      expect(find.text('正在保存 notes.txt'), findsOneWidget);

      backend.emit(BridgeEvent(
          kind: 'done', bytes: BigInt.from(6144), files: BigInt.from(2)));
      await tester.pump();
      expect(find.textContaining('传输完成'), findsOneWidget);
    });
  });

  group('ReceiveScreen failure paths', () {
    testWidgets(
        'wrong words -> claim throws -> error banner (QA failure scenario)',
        (tester) async {
      final backend = FakePairingBackend(
          claimError: Exception('SPAKE2 confirmation mismatch'));
      await tester.pumpWidget(_receiveApp(backend));

      await tester.enterText(find.byType(TextField), '7-wrong-word-here');
      await tester.tap(find.text('连接'));
      await tester.pumpAndSettle();

      expect(find.textContaining('配对失败'), findsOneWidget);
      expect(find.textContaining('confirmation mismatch'), findsOneWidget);
    });

    testWidgets('error event during transfer shows an error banner',
        (tester) async {
      final backend =
          FakePairingBackend(claimResult: FakePairingBackend.demoOffer());
      await tester.pumpWidget(_receiveApp(backend));

      await tester.enterText(find.byType(TextField), _code);
      await tester.tap(find.text('连接'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('接收'));
      await _pumpFrames(tester);

      backend.emit(const BridgeEvent(kind: 'error', message: '连接中断'));
      await tester.pump();

      expect(find.text('传输失败: 连接中断'), findsOneWidget);
    });
  });
}
