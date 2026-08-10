// Home screen smoke test: the two pairing entries render and navigate.

import 'package:flutter_test/flutter_test.dart';

import 'package:app/main.dart';
import 'package:app/screens/receive_screen.dart';
import 'package:app/screens/send_screen.dart';

void main() {
  testWidgets('home screen shows both pairing entries', (tester) async {
    await tester.pumpWidget(MyApp(
      backendFactory: (_) async => throw StateError('not used on home'),
    ));

    expect(find.text('my-croc'), findsOneWidget);
    expect(find.text('发送文件'), findsOneWidget);
    expect(find.text('接收文件'), findsOneWidget);
    expect(find.text('跨网络安全传输文件'), findsOneWidget);
  });

  testWidgets('tapping 发送文件 opens the send screen', (tester) async {
    await tester.pumpWidget(MyApp(
      backendFactory: (_) async => throw StateError('not used on home'),
    ));

    await tester.tap(find.text('发送文件'));
    await tester.pumpAndSettle();

    expect(find.byType(SendScreen), findsOneWidget);
    expect(find.text('选择要发送的文件'), findsOneWidget);
  });

  testWidgets('tapping 接收文件 opens the receive screen', (tester) async {
    await tester.pumpWidget(MyApp(
      backendFactory: (_) async => throw StateError('not used on home'),
    ));

    await tester.tap(find.text('接收文件'));
    await tester.pumpAndSettle();

    expect(find.byType(ReceiveScreen), findsOneWidget);
    expect(find.text('输入配对码'), findsOneWidget);
  });
}
