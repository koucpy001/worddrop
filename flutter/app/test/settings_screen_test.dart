// Widget tests for SettingsScreen — hermetic: injects fake getConfig/setConfig
// callbacks so no native cdylib is needed. Covers load, save, overwrite toggle,
// and error states.
//
// Chinese labels tested: 配对服务器地址, 中继服务器地址, 数据目录, 覆盖已有文件.

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:worddrop/screens/settings_screen.dart';
import 'package:worddrop/src/rust/api/config.dart' as bridge;
import 'package:worddrop/theme.dart';

Widget _wrap(SettingsScreen screen) => MaterialApp(
      theme: buildAppTheme(),
      home: Scaffold(
        appBar: AppBar(title: const Text('设置')),
        body: screen,
      ),
    );

Widget _settingsApp({
  required GetConfigFn getConfig,
  SetConfigFn setConfig = _noopSetConfig,
}) =>
    _wrap(SettingsScreen(getConfig: getConfig, setConfig: setConfig));

Future<String> _noopSetConfig(String key, String value) async => value;

Future<bridge.ConfigDto> _testConfig() async => bridge.ConfigDto(
      rendezvousUrl: 'http://192.168.1.1:8080',
      relayUrl: 'http://192.168.1.1:3340',
      dataDir: '/home/user/.config/worddrop',
      overwrite: false,
    );

Future<void> _pumpFrames(WidgetTester tester, [int frames = 3]) async {
  for (var i = 0; i < frames; i++) {
    await tester.pump(const Duration(milliseconds: 100));
  }
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  group('load config', () {
    testWidgets('renders all four config fields with loaded values',
        (tester) async {
      await tester.pumpWidget(_settingsApp(getConfig: _testConfig));
      await _pumpFrames(tester);

      // Field labels (Chinese).
      expect(find.text('配对服务器地址'), findsOneWidget);
      expect(find.text('中继服务器地址'), findsOneWidget);
      expect(find.text('数据目录'), findsOneWidget);
      expect(find.text('覆盖已有文件'), findsOneWidget);

      // Section labels.
      expect(find.text('网络设置'), findsOneWidget);
      expect(find.text('存储设置'), findsOneWidget);

      // Loaded values in text fields (EditableText may dupe with hint text).
      expect(find.text('http://192.168.1.1:8080'), findsAtLeast(1));
      expect(find.text('http://192.168.1.1:3340'), findsAtLeast(1));
      expect(find.text('/home/user/.config/worddrop'), findsAtLeast(1));

      // Overwrite subtitle.
      expect(
          find.text('接收文件时，如果目标已存在则直接覆盖'), findsOneWidget);
    });

    testWidgets('shows loading spinner while fetching config', (tester) async {
      // A Completer that never completes = perpetual loading spinner.
      final completer = Completer<bridge.ConfigDto>();
      await tester.pumpWidget(_settingsApp(
        getConfig: () => completer.future,
      ));

      expect(find.byType(CircularProgressIndicator), findsOneWidget);
      expect(find.text('配对服务器地址'), findsNothing);
    });

    testWidgets('shows error and retry button on load failure', (tester) async {
      await tester.pumpWidget(_settingsApp(
        getConfig: () async => throw Exception('连接失败'),
      ));
      await _pumpFrames(tester);
      await tester.pumpAndSettle();

      expect(find.textContaining('加载设置失败'), findsOneWidget);
      expect(find.text('重试'), findsOneWidget);
    });
  });

  group('save config', () {
    testWidgets('save button appears on field change and calls setConfig',
        (tester) async {
      String? lastKey;
      String? lastValue;

      await tester.pumpWidget(_settingsApp(
        getConfig: _testConfig,
        setConfig: (key, value) async {
          lastKey = key;
          lastValue = value;
          return value;
        },
      ));
      await _pumpFrames(tester);

      // Type a new rendezvous URL.
      final field = find.widgetWithText(TextField, 'http://192.168.1.1:8080');
      await tester.enterText(field, 'http://new-host:9090');
      await tester.pump();

      // The check (save) button should appear.
      expect(find.byIcon(Icons.check), findsOneWidget);

      // Tap it.
      await tester.tap(find.byIcon(Icons.check));
      await tester.pump();

      expect(lastKey, 'rendezvous_url');
      expect(lastValue, 'http://new-host:9090');
      expect(find.text('已保存'), findsOneWidget);
    });

    testWidgets('submitting via keyboard Enter also saves', (tester) async {
      String? lastValue;

      await tester.pumpWidget(_settingsApp(
        getConfig: _testConfig,
        setConfig: (key, value) async {
          lastValue = value;
          return value;
        },
      ));
      await _pumpFrames(tester);

      final field = find.widgetWithText(TextField, 'http://192.168.1.1:3340');
      await tester.enterText(field, 'http://relay:4000');
      await tester.testTextInput.receiveAction(TextInputAction.done);
      await tester.pump();

      expect(lastValue, 'http://relay:4000');
      expect(find.text('已保存'), findsOneWidget);
    });

    testWidgets('overwrite toggle calls setConfig with true/false',
        (tester) async {
      String? lastKey;
      String? lastValue;

      await tester.pumpWidget(_settingsApp(
        getConfig: _testConfig,
        setConfig: (key, value) async {
          lastKey = key;
          lastValue = value;
          return value;
        },
      ));
      await _pumpFrames(tester);

      // Toggle the switch on (default is false).
      final toggle = find.byType(Switch);
      await tester.tap(toggle);
      await tester.pump();

      expect(lastKey, 'overwrite');
      expect(lastValue, 'true');

      // Toggle off.
      await tester.tap(toggle);
      await tester.pump();

      expect(lastKey, 'overwrite');
      expect(lastValue, 'false');
    });

    testWidgets('save failure shows error snackbar', (tester) async {
      await tester.pumpWidget(_settingsApp(
        getConfig: _testConfig,
        setConfig: (key, value) async => throw Exception('权限不足'),
      ));
      await _pumpFrames(tester);

      final field = find.widgetWithText(TextField, 'http://192.168.1.1:8080');
      await tester.enterText(field, 'http://x');
      await tester.pump();
      await tester.tap(find.byIcon(Icons.check));
      await tester.pump();

      expect(find.textContaining('保存失败'), findsOneWidget);
    });
  });

  group('reload button', () {
    testWidgets('重新加载设置 calls getConfig again and refreshes fields',
        (tester) async {
      var callCount = 0;

      await tester.pumpWidget(_settingsApp(
        getConfig: () async {
          callCount++;
          return _testConfig();
        },
      ));
      await _pumpFrames(tester);

      expect(callCount, 1);

      // Scroll down to the reset button (the settings form is in a ListView).
      await tester.dragUntilVisible(
        find.text('重新加载设置'),
        find.byType(ListView),
        const Offset(0, -200),
      );
      await tester.tap(find.text('重新加载设置'));
      await _pumpFrames(tester);

      expect(callCount, 2);
    });
  });

  group('official server hint (Bug 4 option B)', () {
    Future<bridge.ConfigDto> localDefaults() async => bridge.ConfigDto(
          rendezvousUrl: 'http://127.0.0.1:8080',
          relayUrl: 'http://127.0.0.1:3340',
          dataDir: '/home/user/.config/worddrop',
          overwrite: false,
        );

    testWidgets('hint shown while both fields hold the 127.0.0.1 defaults',
        (tester) async {
      await tester.pumpWidget(_settingsApp(getConfig: localDefaults));
      await _pumpFrames(tester);

      expect(
        find.textContaining('官方服务：https://relay.worddrop.cloud'),
        findsOneWidget,
      );
      expect(
        find.textContaining('https://pair.worddrop.cloud'),
        findsOneWidget,
      );
      // The defaults themselves are unchanged.
      expect(find.text('http://127.0.0.1:8080'), findsAtLeast(1));
      expect(find.text('http://127.0.0.1:3340'), findsAtLeast(1));
    });

    testWidgets('hint disappears as soon as a server field is edited',
        (tester) async {
      await tester.pumpWidget(_settingsApp(getConfig: localDefaults));
      await _pumpFrames(tester);
      expect(find.textContaining('官方服务'), findsOneWidget);

      final field = find.widgetWithText(TextField, 'http://127.0.0.1:8080');
      await tester.enterText(field, 'https://pair.worddrop.cloud');
      await tester.pump();

      expect(find.textContaining('官方服务'), findsNothing);
    });

    testWidgets('hint not shown when config already uses public servers',
        (tester) async {
      await tester.pumpWidget(_settingsApp(getConfig: _testConfig));
      await _pumpFrames(tester);

      expect(find.textContaining('官方服务'), findsNothing);
    });
  });
}
