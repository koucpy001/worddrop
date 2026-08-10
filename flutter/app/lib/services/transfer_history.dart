// Transfer history service — ChangeNotifier that tracks active transfers
// in memory and persists completed/cancelled/failed transfers to
// shared_preferences as a JSON list of {code, names, bytes, time, status}.
//
// Storage choice: shared_preferences (simple, no path_provider dep needed,
// built-in mock for widget tests via SharedPreferences.setMockInitialValues).

import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';

const _historyKey = 'transfer_history';

/// A completed or terminal transfer stored in local history.
class TransferRecord {
  final String code;
  final List<String> names;
  final BigInt bytes;
  final DateTime time;
  final String status; // 'completed', 'cancelled', 'failed'
  final String direction; // 'sent', 'received'

  const TransferRecord({
    required this.code,
    required this.names,
    required this.bytes,
    required this.time,
    required this.status,
    required this.direction,
  });

  Map<String, dynamic> toJson() => {
        'code': code,
        'names': names,
        'bytes': bytes.toString(),
        'time': time.toIso8601String(),
        'status': status,
        'direction': direction,
      };

  factory TransferRecord.fromJson(Map<String, dynamic> json) =>
      TransferRecord(
        code: json['code'] as String,
        names: (json['names'] as List<dynamic>).cast<String>(),
        bytes: BigInt.parse(json['bytes'] as String),
        time: DateTime.parse(json['time'] as String),
        status: json['status'] as String,
        direction: json['direction'] as String? ?? 'sent',
      );
}

/// An in-progress transfer (live, not persisted until terminal).
class ActiveTransfer {
  final String code;
  final List<String> names;
  BigInt totalBytes;
  final String direction;
  final DateTime startTime;
  BigInt received;
  String phase; // 'preparing', 'connecting', 'server_waiting', 'transferring'

  /// Called when the user taps cancel on this transfer card.
  final Future<void> Function()? onCancel;

  ActiveTransfer({
    required this.code,
    required this.names,
    required this.totalBytes,
    required this.direction,
    required this.startTime,
    BigInt? received,
    String? phase,
    this.onCancel,
  })  : received = received ?? BigInt.zero,
        phase = phase ?? 'preparing';

  double? get progress {
    if (totalBytes == BigInt.zero) return null;
    return (received / totalBytes).clamp(0.0, 1.0).toDouble();
  }

  String get statusLabel => switch (phase) {
        'preparing' => '准备中',
        'connecting' => '连接中',
        'server_waiting' => '等待接收方',
        'transferring' => '传输中',
        _ => '进行中',
      };
}

/// Singleton service: active transfers in memory + persisted history.
///
/// Widget tests call [TransferHistory.resetForTest] between tests and use
/// `SharedPreferences.setMockInitialValues({})` for a clean slate.
class TransferHistory extends ChangeNotifier {
  TransferHistory._();

  static final TransferHistory _instance = TransferHistory._();

  /// The singleton instance. Screens and T18 pairing flows both reference this.
  static TransferHistory get instance => _instance;

  final List<ActiveTransfer> _active = [];

  /// History records persisted to shared_preferences.
  List<TransferRecord> history = [];

  /// Whether [load] has been called (avoids duplicate disk reads).
  bool loaded = false;

  List<ActiveTransfer> get active => List.unmodifiable(_active);
  bool get hasActive => _active.isNotEmpty;
  bool get hasHistory => history.isNotEmpty;

  /// Add a new active transfer (called when send/receive begins).
  void addActive(ActiveTransfer transfer) {
    _active.add(transfer);
    notifyListeners();
  }

  /// Update progress bytes on an active transfer (by index or code).
  void updateProgress(String code, BigInt received, [BigInt? total]) {
    final idx = _active.indexWhere((t) => t.code == code);
    if (idx == -1) return;
    _active[idx].received = received;
    if (total != null) _active[idx].totalBytes = total;
    _active[idx].phase = 'transferring';
    notifyListeners();
  }

  /// Update the phase label of an active transfer.
  void updatePhase(String code, String phase) {
    final idx = _active.indexWhere((t) => t.code == code);
    if (idx == -1) return;
    _active[idx].phase = phase;
    notifyListeners();
  }

  /// Move an active transfer to history as completed.
  Future<void> completeTransfer(String code) async {
    final idx = _active.indexWhere((t) => t.code == code);
    if (idx == -1) return;
    final t = _active.removeAt(idx);
    final record = TransferRecord(
      code: t.code,
      names: t.names,
      bytes: t.totalBytes,
      time: DateTime.now(),
      status: 'completed',
      direction: t.direction,
    );
    history.insert(0, record);
    await persistForTest();
    notifyListeners();
  }

  /// Move an active transfer to history as cancelled.
  Future<void> cancelTransfer(String code) async {
    final idx = _active.indexWhere((t) => t.code == code);
    if (idx == -1) return;
    final t = _active.removeAt(idx);
    final record = TransferRecord(
      code: t.code,
      names: t.names,
      bytes: t.received,
      time: DateTime.now(),
      status: 'cancelled',
      direction: t.direction,
    );
    history.insert(0, record);
    await persistForTest();
    notifyListeners();
  }

  /// Move an active transfer to history as failed.
  Future<void> failTransfer(String code) async {
    final idx = _active.indexWhere((t) => t.code == code);
    if (idx == -1) return;
    final t = _active.removeAt(idx);
    final record = TransferRecord(
      code: t.code,
      names: t.names,
      bytes: t.received,
      time: DateTime.now(),
      status: 'failed',
      direction: t.direction,
    );
    history.insert(0, record);
    await persistForTest();
    notifyListeners();
  }

  /// Load persisted history from shared_preferences.
  Future<void> load() async {
    if (loaded) return;
    final prefs = await SharedPreferences.getInstance();
    final raw = prefs.getString(_historyKey);
    if (raw != null) {
      final list = jsonDecode(raw) as List<dynamic>;
      history = list
          .map((e) => TransferRecord.fromJson(e as Map<String, dynamic>))
          .toList();
    }
    loaded = true;
    notifyListeners();
  }

  /// Persist the history list to shared_preferences.
  @visibleForTesting
  Future<void> persistForTest() async {
    final prefs = await SharedPreferences.getInstance();
    final json = jsonEncode(history.map((r) => r.toJson()).toList());
    await prefs.setString(_historyKey, json);
  }

  /// Clear all data (used for test teardown).
  static Future<void> clearForTest() async {
    _instance._active.clear();
    _instance.history.clear();
    _instance.loaded = false;
    _instance.notifyListeners();
    final prefs = await SharedPreferences.getInstance();
    await prefs.remove(_historyKey);
  }

  /// Reset the singleton to a clean state for testing.
  static void resetForTest() {
    _instance._active.clear();
    _instance.history.clear();
    _instance.loaded = false;
  }
}
