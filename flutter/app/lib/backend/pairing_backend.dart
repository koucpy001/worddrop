// Session backend abstraction.
//
// Screens talk to `SessionBackend`, never to the generated FRB bridge
// directly, so widget tests can inject a fake (hermetic `flutter test`, no
// native cdylib needed). One backend instance owns exactly one session.

import 'dart:async';

import 'package:worddrop/src/rust/api/events.dart';
import 'package:worddrop/src/rust/api/session.dart' as bridge;
import 'package:worddrop/src/rust/api/session.dart' show OfferDto, PreparedSendDto;

/// A session endpoint the pairing screens drive.
abstract interface class SessionBackend {
  /// Prepare `paths` on a sender session and return the pairing code.
  Future<PreparedSendDto> prepareSend(List<String> paths);

  /// Claim `code` on a receiver session and return the pending offer.
  Future<OfferDto> claimCode(String code);

  /// Accept the pending offer; empty `targetDir` resolves to the configured
  /// data dir's `received/` subdir (bridge contract).
  Future<void> acceptOffer({String targetDir = ''});

  /// Decline the pending offer.
  Future<void> declineOffer(String reason);

  /// Cancel the session from any non-terminal stage.
  Future<void> cancelSession();

  /// Drop the session and stop its flow.
  Future<void> disposeSession();

  /// Current phase string (created/pending_pair/paired/transferring/
  /// done/cancelled/failed).
  Future<String> sessionPhase();

  /// The session's event stream (progress + phase + errors).
  Stream<BridgeEvent> watchTransfer();
}

/// Creates a session backend for `role`.
typedef SessionBackendFactory = Future<SessionBackend> Function(
    bridge.SessionRole role);

/// Live backend: every call is a real FRB bridge call over the native
/// cdylib. Used by the production app.
class LivePairingBackend implements SessionBackend {
  LivePairingBackend._(this._handle);

  final bridge.SessionHandle _handle;

  static Future<SessionBackend> create(bridge.SessionRole role) async {
    final handle = await bridge.createSession(role: role);
    return LivePairingBackend._(handle);
  }

  @override
  Future<PreparedSendDto> prepareSend(List<String> paths) =>
      bridge.sendPaths(handle: _handle, paths: paths);

  @override
  Future<OfferDto> claimCode(String code) =>
      bridge.receiveTicket(handle: _handle, code: code);

  @override
  Future<void> acceptOffer({String targetDir = ''}) =>
      bridge.acceptOffer(handle: _handle, targetDir: targetDir);

  @override
  Future<void> declineOffer(String reason) =>
      bridge.declineOffer(handle: _handle, reason: reason);

  @override
  Future<void> cancelSession() => bridge.cancelSession(handle: _handle);

  @override
  Future<void> disposeSession() => bridge.disposeSession(handle: _handle);

  @override
  Future<String> sessionPhase() => bridge.sessionPhase(handle: _handle);

  @override
  Stream<BridgeEvent> watchTransfer() =>
      bridge.watchTransfer(handle: _handle);
}
