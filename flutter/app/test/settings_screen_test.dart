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
  CleanupCacheFn cleanupCache = _noopCleanupCache,
}) =>
    _wrap(SettingsScreen(
      getConfig: getConfig,
      setConfig: setConfig,
      cleanupCache: cleanupCache,
    ));

Future<String> _noopSetConfig(String key, String value) async => value;

Future<String> _noopCleanupCache() async => '已清空发送缓存 0 个 blob';

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

  group('default servers hint (public infra defaults)', () {
    Future<bridge.ConfigDto> defaultConfig() async => bridge.ConfigDto(
          rendezvousUrl: 'mqtts://broker.emqx.io:8883',
          relayUrl: 'public',
          dataDir: '/home/user/.config/worddrop',
          overwrite: false,
        );

    testWidgets('hint shown while both fields hold the built-in defaults',
        (tester) async {
      await tester.pumpWidget(_settingsApp(getConfig: defaultConfig));
      await _pumpFrames(tester);

      expect(
        find.textContaining('默认使用公共中继 + 公共配对信箱'),
        findsOneWidget,
      );
      // The defaults themselves are unchanged.
      expect(find.text('mqtts://broker.emqx.io:8883'), findsAtLeast(1));
      expect(find.text('public'), findsAtLeast(1));
    });

    testWidgets(
        'hint disappears when relay is filled with a custom URL',
        (tester) async {
      await tester.pumpWidget(_settingsApp(getConfig: defaultConfig));
      await _pumpFrames(tester);
      expect(find.textContaining('默认使用公共中继'), findsOneWidget);

      final field = find.widgetWithText(TextField, 'public');
      await tester.enterText(field, 'https://relay.worddrop.cloud');
      await tester.pump();

      expect(find.textContaining('默认使用公共中继'), findsNothing);
    });

    testWidgets('hint not shown when config already uses custom servers',
        (tester) async {
      await tester.pumpWidget(_settingsApp(getConfig: _testConfig));
      await _pumpFrames(tester);

      expect(find.textContaining('默认使用公共中继'), findsNothing);
    });
  });

  group('cache cleanup', () {
    /// The cleanup tile sits at the bottom of the settings ListView — scroll
    /// it into view first (lazy-built children are not in the tree otherwise).
    Future<void> scrollToCleanupTile(WidgetTester tester) async {
      await tester.dragUntilVisible(
        find.text('清理缓存'),
        find.byType(ListView),
        const Offset(0, -200),
      );
      await tester.pump();
    }

    testWidgets('renders the 清理缓存 tile with its labels', (tester) async {
      await tester.pumpWidget(_settingsApp(getConfig: _testConfig));
      await _pumpFrames(tester);
      await scrollToCleanupTile(tester);

      expect(find.text('清理缓存'), findsOneWidget);
      expect(find.text('清空发送与接收缓存（不影响已接收的文件）'),
          findsOneWidget);
      expect(find.byIcon(Icons.cleaning_services_outlined), findsOneWidget);
      expect(find.widgetWithText(FilledButton, '清理'), findsOneWidget);
    });

    testWidgets(
        'tapping 清理 opens the confirm dialog; 取消 closes it without cleaning',
        (tester) async {
      var cleanupCalls = 0;
      await tester.pumpWidget(_settingsApp(
        getConfig: _testConfig,
        cleanupCache: () async {
          cleanupCalls++;
          return 'ok';
        },
      ));
      await _pumpFrames(tester);
      await scrollToCleanupTile(tester);

      await tester.tap(find.text('清理'));
      await tester.pumpAndSettle();

      expect(find.byType(AlertDialog), findsOneWidget);
      expect(
        find.text('确定清理缓存？将清空发送与接收的传输缓存，已接收的文件不受影响。'),
        findsOneWidget,
      );
      expect(find.text('取消'), findsOneWidget);

      await tester.tap(find.text('取消'));
      await tester.pumpAndSettle();

      expect(find.byType(AlertDialog), findsNothing);
      expect(cleanupCalls, 0);
    });

    testWidgets('confirming calls cleanupCache and shows the returned stats',
        (tester) async {
      await tester.pumpWidget(_settingsApp(
        getConfig: _testConfig,
        cleanupCache: () async =>
            '已清空发送缓存 2 个 blob / Cleared send cache (2 blobs)',
      ));
      await _pumpFrames(tester);
      await scrollToCleanupTile(tester);

      await tester.tap(find.text('清理'));
      await tester.pumpAndSettle();
      await tester.tap(find.descendant(
          of: find.byType(AlertDialog), matching: find.text('清理')));
      await tester.pumpAndSettle();

      expect(find.byType(AlertDialog), findsNothing);
      expect(find.textContaining('已清空发送缓存 2 个 blob'), findsOneWidget);
    });

    testWidgets('cleanup failure shows an error snackbar', (tester) async {
      await tester.pumpWidget(_settingsApp(
        getConfig: _testConfig,
        cleanupCache: () async => throw Exception('有活跃传输，请完成后再清理'),
      ));
      await _pumpFrames(tester);
      await scrollToCleanupTile(tester);

      await tester.tap(find.text('清理'));
      await tester.pumpAndSettle();
      await tester.tap(find.descendant(
          of: find.byType(AlertDialog), matching: find.text('清理')));
      await tester.pumpAndSettle();

      expect(find.textContaining('清理失败'), findsOneWidget);
    });

    testWidgets('tile shows a spinner and disables while cleanup is in flight',
        (tester) async {
      final completer = Completer<String>();
      await tester.pumpWidget(_settingsApp(
        getConfig: _testConfig,
        cleanupCache: () => completer.future,
      ));
      await _pumpFrames(tester);
      await scrollToCleanupTile(tester);

      await tester.tap(find.text('清理'));
      await tester.pumpAndSettle();
      await tester.tap(find.descendant(
          of: find.byType(AlertDialog), matching: find.text('清理')));
      // Fixed pumps for the dialog exit — the spinner never settles.
      await tester.pump(const Duration(milliseconds: 100));
      await tester.pump(const Duration(milliseconds: 300));

      expect(find.widgetWithText(FilledButton, '清理'), findsNothing);
      expect(find.byType(CircularProgressIndicator), findsOneWidget);
    });
  });
}
