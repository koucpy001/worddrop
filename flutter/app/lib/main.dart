// WordDrop GUI entry. Wires the live bridge backend; the `WORDDROP_DEMO_MODE`
// env var switches to the scripted demo backend for headless manual QA of
// every screen state without a peer.

import 'dart:io';

import 'package:flutter/material.dart';

import 'package:worddrop/backend/fake_backend.dart';
import 'package:worddrop/backend/pairing_backend.dart';
import 'package:worddrop/screens/home_screen.dart';
import 'package:worddrop/src/rust/api/session.dart' as bridge;
import 'package:worddrop/src/rust/frb_generated.dart';
import 'package:worddrop/theme.dart';

bool get _demoMode => Platform.environment['WORDDROP_DEMO_MODE'] == 'true';

Future<SessionBackend> _backendFactory(bridge.SessionRole role) async {
  if (_demoMode) return DemoPairingBackend(role);
  return LivePairingBackend.create(role);
}

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  try {
    await RustLib.init();
  } catch (error) {
    if (!_demoMode) rethrow;
    // Demo mode must run even without the native cdylib.
    debugPrint('[main] demo mode: bridge init skipped ($error)');
  }
  runApp(MyApp(backendFactory: _backendFactory));
}

class MyApp extends StatelessWidget {
  const MyApp({super.key, required this.backendFactory});

  final SessionBackendFactory backendFactory;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'WordDrop',
      debugShowCheckedModeBanner: false,
      theme: buildAppTheme(),
      home: HomeScreen(backendFactory: backendFactory),
    );
  }
}
