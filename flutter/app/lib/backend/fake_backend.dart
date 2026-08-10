// Fake and demo session backends.
//
// FakePairingBackend: deterministic, test-controlled (Completer-driven) —
// used by widget tests so `flutter test` stays hermetic (no native cdylib,
// no rendezvous). DemoPairingBackend: scripted timeline for the manual QA
// demo mode (MY_CROC_DEMO_MODE dart-define) so every screen state can be
// screenshotted headlessly without a real peer.

import 'dart:async';

import 'package:app/backend/pairing_backend.dart';
import 'package:app/src/rust/api/events.dart';
import 'package:app/src/rust/api/session.dart'
    show FileMetaDto, OfferDto, PreparedSendDto, SessionRole;

/// Demo pairing code (fixed so screenshots are deterministic).
const String demoCode = '7-correct-horse-battery';

/// Test-controlled fake. Configure either immediate results or Completers;
/// watch every call and emit events at will via [emit].
class FakePairingBackend implements SessionBackend {
  FakePairingBackend({
    this.prepareResult,
    this.claimResult,
    this.claimError,
    this.prepareError,
  });

  /// Immediate results (when set, Completers are ignored).
  PreparedSendDto? prepareResult;
  OfferDto? claimResult;
  Object? claimError;
  Object? prepareError;

  /// Deferred control: when set, the call awaits this future.
  Completer<PreparedSendDto>? sendCompleter;
  Completer<OfferDto>? claimCompleter;

  // Call log.
  List<String>? lastPaths;
  String? lastCode;
  String? lastTargetDir;
  String? lastDeclineReason;
  bool cancelCalled = false;
  bool disposeCalled = false;

  final StreamController<BridgeEvent> _controller =
      StreamController<BridgeEvent>.broadcast(sync: true);

  @override
  Future<PreparedSendDto> prepareSend(List<String> paths) async {
    lastPaths = paths;
    if (prepareError != null) throw prepareError!;
    final completer = sendCompleter;
    if (completer != null) return completer.future;
    final result = prepareResult;
    if (result != null) return result;
    throw StateError('FakePairingBackend: prepareSend not configured');
  }

  @override
  Future<OfferDto> claimCode(String code) async {
    lastCode = code;
    if (claimError != null) throw claimError!;
    final completer = claimCompleter;
    if (completer != null) return completer.future;
    final result = claimResult;
    if (result != null) return result;
    throw StateError('FakePairingBackend: claimCode not configured');
  }

  @override
  Future<void> acceptOffer({String targetDir = ''}) async {
    lastTargetDir = targetDir;
  }

  @override
  Future<void> declineOffer(String reason) async {
    lastDeclineReason = reason;
  }

  @override
  Future<void> cancelSession() async {
    cancelCalled = true;
  }

  @override
  Future<void> disposeSession() async {
    disposeCalled = true;
  }

  @override
  Future<String> sessionPhase() async => 'created';

  @override
  Stream<BridgeEvent> watchTransfer() => _controller.stream;

  /// Push an event into the watched stream (sync delivery).
  void emit(BridgeEvent event) => _controller.add(event);

  /// Build a demo offer of two files.
  static OfferDto demoOffer() => OfferDto(
        files: [
          FileMetaDto(name: 'demo-photos.zip', size: BigInt.from(2048), hash: 'aa'),
          FileMetaDto(name: 'notes.txt', size: BigInt.from(4096), hash: 'bb'),
        ],
        totalBytes: BigInt.from(6144),
      );
}

/// Scripted demo flow for manual QA (demo mode). No real bridge.
class DemoPairingBackend implements SessionBackend {
  /// `role` is accepted for API symmetry but the scripted flow is identical
  /// for both sides.
  DemoPairingBackend(SessionRole role);

  final StreamController<BridgeEvent> _controller =
      StreamController<BridgeEvent>.broadcast();
  final List<Timer> _timers = [];
  bool _disposed = false;

  void _after(Duration delay, void Function() action) {
    if (_disposed) return;
    _timers.add(Timer(delay, action));
  }

  void _emit(BridgeEvent event) => _controller.add(event);

  @override
  Future<PreparedSendDto> prepareSend(List<String> paths) async {
    _emit(BridgeEvent(
        kind: 'file_found',
        name: 'demo-photos.zip',
        total: BigInt.from(2048)));
    _emit(BridgeEvent(
        kind: 'file_found', name: 'notes.txt', total: BigInt.from(4096)));
    _after(const Duration(milliseconds: 400), () {
      _emit(const BridgeEvent(kind: 'file_imported', name: 'demo-photos.zip'));
    });
    _after(const Duration(milliseconds: 900), () {
      _emit(const BridgeEvent(kind: 'file_imported', name: 'notes.txt'));
    });
    await Future<void>.delayed(const Duration(milliseconds: 1200));
    return PreparedSendDto(
      code: demoCode,
      files: [
        FileMetaDto(name: 'demo-photos.zip', size: BigInt.from(2048), hash: 'aa'),
        FileMetaDto(name: 'notes.txt', size: BigInt.from(4096), hash: 'bb'),
      ],
      totalBytes: BigInt.from(6144),
    );
  }

  @override
  Future<OfferDto> claimCode(String code) async {
    _after(const Duration(milliseconds: 600),
        () => _emit(const BridgeEvent(kind: 'connecting')));
    await Future<void>.delayed(const Duration(milliseconds: 1000));
    return FakePairingBackend.demoOffer();
  }

  @override
  Future<void> acceptOffer({String targetDir = ''}) async {
    final total = BigInt.from(6144);
    final steps = [BigInt.from(1024), BigInt.from(3072), total];
    for (var i = 0; i < steps.length; i++) {
      final step = steps[i];
      _after(Duration(milliseconds: 600 * (i + 1)), () {
        _emit(BridgeEvent(kind: 'downloading', received: step, total: total));
      });
    }
    _after(const Duration(milliseconds: 2400),
        () => _emit(const BridgeEvent(kind: 'exporting', name: 'demo-photos.zip')));
    _after(const Duration(milliseconds: 3000),
        () => _emit(const BridgeEvent(kind: 'exporting', name: 'notes.txt')));
    _after(const Duration(milliseconds: 3600), () {
      _emit(BridgeEvent(kind: 'done', bytes: total, files: BigInt.from(2)));
    });
  }

  @override
  Future<void> declineOffer(String reason) async {
    _emit(BridgeEvent(kind: 'info', message: '已拒绝传输: $reason'));
  }

  @override
  Future<void> cancelSession() async {
    _emit(const BridgeEvent(kind: 'phase', phase: 'cancelled'));
  }

  @override
  Future<void> disposeSession() async {
    _disposed = true;
    for (final timer in _timers) {
      timer.cancel();
    }
    await _controller.close();
  }

  @override
  Future<String> sessionPhase() async => 'created';

  @override
  Stream<BridgeEvent> watchTransfer() => _controller.stream;
}
