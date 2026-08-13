// Unit tests for the transfer-history security fix:
//   - the pairing word-code (the SPAKE2 password) is NEVER persisted —
//     records serialize with a numeric `nameplate` instead of the full `code`
//   - legacy records carrying the old `code` field are migrated on load:
//     `code` is stripped, `nameplate` derived from its prefix, and the
//     sanitized JSON is written back (idempotent)
//   - corrupt entries are quarantined (dropped + logged) — load never throws

import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:worddrop/services/transfer_history.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  setUp(() async {
    SharedPreferences.setMockInitialValues({});
    TransferHistory.resetForTest();
  });

  tearDown(() async {
    await TransferHistory.clearForTest();
  });

  TransferRecord makeRecord({String nameplate = '4173'}) => TransferRecord(
        nameplate: nameplate,
        names: ['photo.zip'],
        bytes: BigInt.from(1048576),
        time: DateTime.utc(2026, 8, 12),
        status: 'completed',
        direction: 'received',
      );

  group('serialization', () {
    test('record serializes with nameplate and NEVER with the word-code', () {
      final json = makeRecord().toJson();
      expect(json.containsKey('nameplate'), isTrue);
      expect(json['nameplate'], '4173');
      expect(json.containsKey('code'), isFalse);
      // The remaining fields are preserved.
      expect(json['names'], ['photo.zip']);
      expect(json['bytes'], '1048576');
      expect(json['status'], 'completed');
      expect(json['direction'], 'received');
    });
  });

  group('corrupt record quarantine', () {
    test('load returns the valid entries and does not throw on a corrupt one',
        () async {
      SharedPreferences.setMockInitialValues({
        'transfer_history': jsonEncode([
          {
            'nameplate': '7',
            'names': ['ok.txt'],
            'bytes': '1024',
            'time': '2026-08-12T10:00:00.000',
            'status': 'completed',
            'direction': 'sent',
          },
          {
            'nameplate': 'broken',
            'names': 'not-a-list', // invalid: names must be a list
            'bytes': '1024',
            'time': '2026-08-12T10:00:00.000',
            'status': 'completed',
            'direction': 'sent',
          },
        ]),
      });

      await TransferHistory.instance.load(); // must not throw

      final history = TransferHistory.instance.history;
      expect(history, hasLength(1));
      expect(history.single.nameplate, '7');
      expect(history.single.names, ['ok.txt']);
    });

    test('load does not throw on a fully corrupt blob', () async {
      SharedPreferences.setMockInitialValues({
        'transfer_history': 'not json at all',
      });

      await TransferHistory.instance.load();

      expect(TransferHistory.instance.history, isEmpty);
    });

    test('load does not throw when the blob is not a list', () async {
      SharedPreferences.setMockInitialValues({
        'transfer_history': '{"oops": true}',
      });

      await TransferHistory.instance.load();

      expect(TransferHistory.instance.history, isEmpty);
    });
  });

  group('legacy migration', () {
    test('legacy code field is stripped and nameplate derived from its prefix',
        () async {
      SharedPreferences.setMockInitialValues({
        'transfer_history': jsonEncode([
          {
            'code': '4173-caretaker-fascinate-cellulose',
            'names': ['old.bin'],
            'bytes': '2048',
            'time': '2026-08-01T10:00:00.000',
            'status': 'failed',
            'direction': 'sent',
          },
        ]),
      });

      await TransferHistory.instance.load();

      final history = TransferHistory.instance.history;
      expect(history, hasLength(1));
      expect(history.single.nameplate, '4173');

      // The sanitized record is written back: no word-code remains on disk.
      final prefs = await SharedPreferences.getInstance();
      final stored = prefs.getString('transfer_history')!;
      expect(stored, contains('nameplate'));
      expect(stored, contains('4173'));
      expect(stored, isNot(contains('caretaker')));
      expect(stored, isNot(contains('"code"')));
    });

    test('migration is idempotent — reloading sanitized records is harmless',
        () async {
      SharedPreferences.setMockInitialValues({
        'transfer_history': jsonEncode([
          {
            'code': '42-correct-horse-battery',
            'names': ['a.txt'],
            'bytes': '512',
            'time': '2026-08-01T10:00:00.000',
            'status': 'completed',
            'direction': 'received',
          },
        ]),
      });

      await TransferHistory.instance.load();
      final first = TransferHistory.instance.history.single;

      TransferHistory.resetForTest();
      await TransferHistory.instance.load(); // reload the sanitized JSON

      final second = TransferHistory.instance.history.single;
      expect(second.nameplate, '42');
      expect(second.names, first.names);
      expect(second.bytes, first.bytes);
      expect(second.status, first.status);
      expect(second.direction, first.direction);

      final prefs = await SharedPreferences.getInstance();
      expect(prefs.getString('transfer_history'), isNot(contains('"code"')));
    });

    test('a record with neither nameplate nor legacy code is quarantined',
        () async {
      SharedPreferences.setMockInitialValues({
        'transfer_history': jsonEncode([
          {'names': ['x.txt'], 'bytes': '1', 'status': 'failed'},
        ]),
      });

      await TransferHistory.instance.load();

      expect(TransferHistory.instance.history, isEmpty);
    });
  });
}
