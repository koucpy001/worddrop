// Widget tests for TransfersScreen — hermetic: injects TransferHistory state
// via the shared singleton (no native bridge needed). Uses
// SharedPreferences.setMockInitialValues for persistence round-trips.
//
// Covered states:
//   - empty list (no active, no history)
//   - active transfers (progress bar, status chip, cancel button)
//   - history entries (completed, cancelled, failed with Chinese labels)
//   - persistence round-trip (add -> load confirms in list)

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:app/screens/transfers_screen.dart';
import 'package:app/services/transfer_history.dart';
import 'package:app/theme.dart';

Widget _wrapApp() => MaterialApp(
      theme: buildAppTheme(),
      home: Scaffold(
        appBar: AppBar(title: const Text('传输列表')),
        body: const TransfersScreen(),
      ),
    );

/// A widget that just wraps the transfers body (no AppBar) for
/// targeted card assertions.

Future<void> _pumpFrames(WidgetTester tester, [int frames = 3]) async {
  for (var i = 0; i < frames; i++) {
    await tester.pump(const Duration(milliseconds: 100));
  }
}

final _k6144 = BigInt.from(6144);

ActiveTransfer _active(String code, String direction,
    {BigInt? received, BigInt? total}) {
  return ActiveTransfer(
    code: code,
    names: ['a.txt', 'b.bin'],
    totalBytes: total ?? _k6144,
    direction: direction,
    startTime: DateTime.now().subtract(const Duration(minutes: 1)),
    received: received,
    phase: (received ?? BigInt.zero) > BigInt.zero ? 'transferring' : 'preparing',
  );
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

  group('empty state', () {
    testWidgets('shows placeholder when no active or history transfers',
        (tester) async {
      await tester.pumpWidget(_wrapApp());
      await tester.pumpAndSettle();

      expect(find.text('暂无传输记录'), findsOneWidget);
      expect(find.text('完成一次传输后，记录将显示在这里'), findsOneWidget);
      expect(find.text('进行中'), findsNothing);
    });
  });

  group('active transfers', () {
    testWidgets('shows active transfer with code, names, progress, cancel',
        (tester) async {
      final store = TransferHistory.instance;
      store.addActive(_active('7-correct-horse-battery', 'sent',
          received: BigInt.from(3072)));
      await tester.pumpWidget(_wrapApp());
      await _pumpFrames(tester);

      expect(find.text('进行中'), findsOneWidget);
      expect(find.text('传输中'), findsOneWidget); // status chip
      expect(find.text('a.txt、b.bin'), findsOneWidget);
      expect(find.textContaining('7-correct-horse-battery'), findsOneWidget);
      expect(find.textContaining('3.0 KiB'), findsAtLeast(1));
      // '6.0 KiB' appears in the total size AND the progress "3.0 KiB / 6.0 KiB (50%)".
      expect(find.textContaining('6.0 KiB'), findsAtLeast(1));
      expect(find.text('取消'), findsOneWidget);
      // Progress bar exists.
      expect(find.byType(LinearProgressIndicator), findsOneWidget);
    });

    testWidgets('cancel button removes active and adds cancelled to history',
        (tester) async {
      final store = TransferHistory.instance;
      store.addActive(_active('7-correct-horse-battery', 'received'));
      await tester.pumpWidget(_wrapApp());
      await _pumpFrames(tester);

      await tester.tap(find.text('取消'));
      await tester.pumpAndSettle();

      // Active becomes history with '已取消' label; only the numeric
      // nameplate is shown (the word-code is never kept around).
      expect(find.text('进行中'), findsNothing);
      expect(find.text('已取消'), findsOneWidget);
      expect(find.text('7'), findsOneWidget);
      expect(find.textContaining('7-correct-horse-battery'), findsNothing);
    });

    testWidgets('multiple active transfers render in order', (tester) async {
      final store = TransferHistory.instance;
      store.addActive(_active('1-alpha-beta-gamma', 'sent',
          received: BigInt.from(1024)));
      store.addActive(_active('2-delta-epsilon-zeta', 'received',
          received: BigInt.zero, total: BigInt.from(8192)));
      await tester.pumpWidget(_wrapApp());
      await _pumpFrames(tester);

      // Both codes visible.
      expect(find.textContaining('1-alpha-beta-gamma'), findsOneWidget);
      expect(find.textContaining('2-delta-epsilon-zeta'), findsOneWidget);
      // Both have cancel buttons.
      expect(find.text('取消'), findsNWidgets(2));
    });
  });

  group('history entries', () {
    testWidgets('completed transfer shows with 已完成 status', (tester) async {
      final store = TransferHistory.instance;
      store.history.add(TransferRecord(
        nameplate: '7',
        names: ['photo.zip'],
        bytes: BigInt.from(1048576),
        time: DateTime.now().subtract(const Duration(hours: 2)),
        status: 'completed',
        direction: 'received',
      ));
      store.loaded = true;
      await tester.pumpWidget(_wrapApp());
      await _pumpFrames(tester);

      expect(find.text('已完成'), findsOneWidget);
      expect(find.text('photo.zip'), findsOneWidget);
      expect(find.text('1.0 MiB'), findsOneWidget);
      expect(find.text('7'), findsOneWidget);
      // No cancel buttons (history only).
      expect(find.text('取消'), findsNothing);
    });

    testWidgets('cancelled transfer shows with 已取消 status', (tester) async {
      final store = TransferHistory.instance;
      store.history.add(TransferRecord(
        nameplate: '3',
        names: ['notes.txt'],
        bytes: BigInt.from(512),
        time: DateTime.now().subtract(const Duration(minutes: 5)),
        status: 'cancelled',
        direction: 'sent',
      ));
      store.loaded = true;
      await tester.pumpWidget(_wrapApp());
      await _pumpFrames(tester);

      expect(find.text('已取消'), findsOneWidget);
      expect(find.text('notes.txt'), findsOneWidget);
      expect(find.text('512 B'), findsOneWidget);
    });

    testWidgets('failed transfer shows with 失败 status', (tester) async {
      final store = TransferHistory.instance;
      store.history.add(TransferRecord(
        nameplate: '5',
        names: ['large.bin'],
        bytes: BigInt.zero,
        time: DateTime.now().subtract(const Duration(days: 1)),
        status: 'failed',
        direction: 'received',
      ));
      store.loaded = true;
      await tester.pumpWidget(_wrapApp());
      await _pumpFrames(tester);

      expect(find.text('失败'), findsOneWidget);
      expect(find.text('0 B'), findsOneWidget);
    });

    testWidgets('mixed active + history shows both sections', (tester) async {
      final store = TransferHistory.instance;
      store.addActive(_active('4-stu-vwx-yza', 'sent',
          received: BigInt.from(2048)));
      store.history.add(TransferRecord(
        nameplate: '6',
        names: ['doc.pdf'],
        bytes: BigInt.from(204800),
        time: DateTime.now().subtract(const Duration(days: 2)),
        status: 'completed',
        direction: 'sent',
      ));
      store.loaded = true;
      await tester.pumpWidget(_wrapApp());
      await _pumpFrames(tester);

      // Both section labels visible.
      expect(find.text('进行中'), findsOneWidget);
      expect(find.text('传输记录'), findsOneWidget);
      // Active has cancel.
      expect(find.text('取消'), findsOneWidget);
      // History entry shows.
      expect(find.text('已完成'), findsOneWidget);
    });
  });

  group('persistence round-trip', () {
    testWidgets(
        'history survives reload — add record, reload store, verify retained',
        (tester) async {
      // Step 1: add a record.
      final store = TransferHistory.instance;
      final record = TransferRecord(
        nameplate: '7',
        names: ['file.bin'],
        bytes: BigInt.from(4096),
        time: DateTime.now(),
        status: 'completed',
        direction: 'sent',
      );
      store.history.add(record);
      store.loaded = true;
      await store.persistForTest();

      // Step 2: reset and reload.
      TransferHistory.resetForTest();
      await TransferHistory.instance.load();

      // Step 3: verify the record is back.
      expect(TransferHistory.instance.history, hasLength(1));
      final loaded = TransferHistory.instance.history.first;
      expect(loaded.nameplate, '7');
      expect(loaded.names, ['file.bin']);
      expect(loaded.bytes, BigInt.from(4096));
      expect(loaded.status, 'completed');
      expect(loaded.direction, 'sent');
    });

    testWidgets('multiple records persist and reload in order',
        (tester) async {
      final store = TransferHistory.instance;
      store.history.add(TransferRecord(
        nameplate: '1',
        names: ['a.txt'],
        bytes: BigInt.from(100),
        time: DateTime.now(),
        status: 'completed',
        direction: 'sent',
      ));
      store.history.add(TransferRecord(
        nameplate: '2',
        names: ['b.txt'],
        bytes: BigInt.from(200),
        time: DateTime.now(),
        status: 'cancelled',
        direction: 'received',
      ));
      store.loaded = true;
      await store.persistForTest();

      TransferHistory.resetForTest();
      await TransferHistory.instance.load();

      final history = TransferHistory.instance.history;
      expect(history, hasLength(2));
      expect(history[0].nameplate, '1'); // first added = first
      expect(history[1].status, 'cancelled');
    });
  });
}
