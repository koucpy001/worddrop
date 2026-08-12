// Send screen: pick files -> show the pairing code LARGE + copy button +
// waiting status -> transfer progress / done / error.

import 'dart:async';
import 'dart:io';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'package:app/backend/pairing_backend.dart';
import 'package:app/services/transfer_history.dart';
import 'package:app/src/rust/api/events.dart';
import 'package:app/src/rust/api/session.dart' show PreparedSendDto, SessionRole;
import 'package:app/theme.dart';
import 'package:app/util/format.dart';
import 'package:app/widgets/status_banner.dart';

/// Picks files and returns their paths (null when cancelled).
typedef FilePickFn = Future<List<String>?> Function();

/// Default picker: the file_picker plugin's multi-file dialog. In demo mode
/// (WORDDROP_DEMO_MODE, headless manual QA without the XDG portal) a fixed
/// fixture path stands in for the dialog.
Future<List<String>?> defaultPickFiles() async {
  if (Platform.environment['WORDDROP_DEMO_MODE'] == 'true') {
    const demoFile = '/tmp/opencode/qa/sendme.txt';
    return [demoFile];
  }
  final result = await FilePicker.pickFiles(
    allowMultiple: true,
    dialogTitle: '选择要发送的文件',
  );
  return result?.paths.whereType<String>().toList() ?? const [];
}

enum SendStage { idle, preparing, waiting, transferring, done, failed, cancelled }

class SendScreen extends StatefulWidget {
  const SendScreen({
    super.key,
    required this.backendFactory,
    this.pickFiles = defaultPickFiles,
  });

  final SessionBackendFactory backendFactory;

  /// Injectable so widget tests never touch the native picker.
  final FilePickFn pickFiles;

  @override
  State<SendScreen> createState() => _SendScreenState();
}

class _SendScreenState extends State<SendScreen> {
  SendStage _stage = SendStage.idle;
  SessionBackend? _backend;
  StreamSubscription<BridgeEvent>? _events;
  PreparedSendDto? _prepared;
  BigInt? _received;
  BigInt? _total;
  int _filesFound = 0;
  String? _error;

  @override
  void dispose() {
    // frb streams: never await subscription.cancel() (never completes in the
    // Dart VM — T16) — drop the session to stop the Rust flow instead.
    _events?.cancel();
    _backend?.disposeSession();
    super.dispose();
  }

  Future<SessionBackend> _ensureBackend() async {
    var backend = _backend;
    if (backend == null) {
      backend = await widget.backendFactory(SessionRole.sender);
      _backend = backend;
      _events = backend.watchTransfer().listen(_onEvent);
    }
    return backend;
  }

  Future<void> _pickAndSend() async {
    final paths = await widget.pickFiles();
    if (paths == null || paths.isEmpty || !mounted) return;
    setState(() {
      _stage = SendStage.preparing;
      _filesFound = 0;
      _error = null;
    });
    try {
      final backend = await _ensureBackend();
      final prepared = await backend.prepareSend(paths);
      if (!mounted) return;
      setState(() {
        _prepared = prepared;
        _stage = SendStage.waiting;
      });
    } catch (error) {
      if (!mounted) return;
      setState(() {
        _stage = SendStage.failed;
        _error = '准备发送失败: $error';
      });
    }
  }

  void _onEvent(BridgeEvent event) {
    if (!mounted) return;
    final store = TransferHistory.instance;
    switch (event.kind) {
      case 'file_imported':
        setState(() => _filesFound++);
      case 'served':
        final prepared = _prepared;
        if (prepared != null) {
          if (_stage != SendStage.transferring) {
            setState(() => _stage = SendStage.transferring);
            // First serving event: register the live transfer so the
            // transfers screen shows an active card with a cancel button.
            store.addActive(ActiveTransfer(
              code: prepared.code,
              names: prepared.files.map((f) => f.name).toList(),
              totalBytes: prepared.totalBytes,
              direction: 'sent',
              startTime: DateTime.now(),
              onCancel: _cancel,
            ));
          }
          store.updateProgress(
              prepared.code, event.received ?? BigInt.zero, event.total);
        }
        setState(() {
          _received = event.received;
          _total = event.total;
        });
      case 'done':
        final prepared = _prepared;
        if (prepared != null) store.completeTransfer(prepared.code);
        setState(() => _stage = SendStage.done);
      case 'phase':
        final prepared = _prepared;
        if (event.phase == 'cancelled') {
          if (prepared != null) store.cancelTransfer(prepared.code);
          setState(() => _stage = SendStage.cancelled);
        } else if (event.phase == 'done') {
          if (prepared != null) store.completeTransfer(prepared.code);
          setState(() => _stage = SendStage.done);
        }
      case 'error':
        final prepared = _prepared;
        if (prepared != null) store.failTransfer(prepared.code);
        setState(() {
          _stage = SendStage.failed;
          _error = '传输失败: ${event.message ?? '未知错误'}';
        });
      default:
        break;
    }
  }

  Future<void> _cancel() async {
    try {
      await _backend?.cancelSession();
    } catch (_) {
      // Event stream carries the phase anyway; ignore late failures.
    }
    // Record the cancellation even if the phase event is lost.
    final prepared = _prepared;
    if (prepared != null) {
      await TransferHistory.instance.cancelTransfer(prepared.code);
    }
    if (mounted) setState(() => _stage = SendStage.cancelled);
  }

  Future<void> _copyCode() async {
    final code = _prepared?.code;
    if (code == null) return;
    await Clipboard.setData(ClipboardData(text: code));
    if (!mounted) return;
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(const SnackBar(content: Text('已复制配对码')));
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('发送文件')),
      body: SafeArea(
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 440),
            child: Padding(
              padding: const EdgeInsets.all(AppSpacing.lg),
              child: switch (_stage) {
                SendStage.idle => _IdleBody(onPick: _pickAndSend),
                SendStage.preparing => _PreparingBody(found: _filesFound),
                SendStage.waiting => _WaitingBody(
                    prepared: _prepared!,
                    onCopy: _copyCode,
                    onCancel: _cancel,
                  ),
                SendStage.transferring => _TransferringBody(
                    received: _received,
                    total: _total,
                    onCancel: _cancel,
                  ),
                SendStage.done => const StatusBanner(
                    variant: BannerVariant.success,
                    message: '传输完成',
                  ),
                SendStage.failed => StatusBanner(
                    variant: BannerVariant.error,
                    message: _error ?? '传输失败',
                    actionLabel: '返回',
                    onAction: () => Navigator.of(context).pop(),
                  ),
                SendStage.cancelled => StatusBanner(
                    variant: BannerVariant.cancelled,
                    message: '已取消发送',
                    actionLabel: '返回',
                    onAction: () => Navigator.of(context).pop(),
                  ),
              },
            ),
          ),
        ),
      ),
    );
  }
}

class _IdleBody extends StatelessWidget {
  const _IdleBody({required this.onPick});

  final VoidCallback onPick;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        Icon(Icons.folder_open_outlined,
            size: 72, color: scheme.primary.withValues(alpha: 0.7)),
        const SizedBox(height: AppSpacing.lg),
        const Text(
          '选择要发送的文件',
          style: TextStyle(
              fontSize: 18, fontWeight: FontWeight.w600, color: AppColors.ink),
        ),
        const SizedBox(height: AppSpacing.sm),
        const Text(
          '发送后你会得到一个配对码，对方输入该配对码即可接收',
          textAlign: TextAlign.center,
          style: TextStyle(fontSize: 13.5, color: Color(0xFF78716C), height: 1.5),
        ),
        const SizedBox(height: AppSpacing.xl),
        FilledButton.icon(
          onPressed: onPick,
          icon: const Icon(Icons.add),
          label: const Text('选择文件'),
        ),
      ],
    );
  }
}

class _PreparingBody extends StatelessWidget {
  const _PreparingBody({required this.found});

  final int found;

  @override
  Widget build(BuildContext context) {
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        const SizedBox(
          width: 32,
          height: 32,
          child: CircularProgressIndicator(strokeWidth: 3),
        ),
        const SizedBox(height: AppSpacing.lg),
        Text(
          found == 0 ? '正在准备文件...' : '正在准备文件... 已添加 $found 个',
          style: const TextStyle(
              fontSize: 15, fontWeight: FontWeight.w500, color: AppColors.ink),
        ),
      ],
    );
  }
}

class _WaitingBody extends StatelessWidget {
  const _WaitingBody({
    required this.prepared,
    required this.onCopy,
    required this.onCancel,
  });

  final PreparedSendDto prepared;
  final VoidCallback onCopy;
  final VoidCallback onCancel;

  @override
  Widget build(BuildContext context) {
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(
          '共 ${prepared.files.length} 个文件 · ${humanBytes(prepared.totalBytes)}',
          textAlign: TextAlign.center,
          style: const TextStyle(fontSize: 13, color: Color(0xFF78716C)),
        ),
        const SizedBox(height: AppSpacing.md),
        _CodeCard(code: prepared.code),
        const SizedBox(height: AppSpacing.lg),
        OutlinedButton.icon(
          onPressed: onCopy,
          icon: const Icon(Icons.copy_outlined, size: 18),
          label: const Text('复制配对码'),
        ),
        const SizedBox(height: AppSpacing.lg),
        Row(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            const SizedBox(
              width: 12,
              height: 12,
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
            const SizedBox(width: AppSpacing.sm + 2),
            const Text(
              '等待接收方输入...',
              style: TextStyle(fontSize: 14, color: Color(0xFF57534E)),
            ),
          ],
        ),
        const SizedBox(height: AppSpacing.lg),
        TextButton(
          onPressed: onCancel,
          child: const Text('取消发送', style: TextStyle(color: Color(0xFFB3261E))),
        ),
      ],
    );
  }
}

/// The pairing code, split into chips: the nameplate chip is visually
/// distinct from the three word chips — the layout encodes the security
/// model (only the nameplate ever leaves the device, Oracle F1).
class _CodeCard extends StatelessWidget {
  const _CodeCard({required this.code});

  final String code;

  @override
  Widget build(BuildContext context) {
    final parts = code.split('-');
    final nameplate = parts.isNotEmpty ? parts.first : code;
    final words = parts.skip(1).toList();
    return Container(
      padding: const EdgeInsets.all(AppSpacing.lg),
      decoration: BoxDecoration(
        color: AppColors.codeBg,
        borderRadius: BorderRadius.circular(AppRadius.lg),
        border: Border.all(color: const Color(0xFFD6E2D0)),
      ),
      child: Column(
        children: [
          const Text(
            '配对码',
            style: TextStyle(fontSize: 13, color: Color(0xFF57534E)),
          ),
          const SizedBox(height: AppSpacing.md),
          Wrap(
            spacing: AppSpacing.sm,
            runSpacing: AppSpacing.sm,
            alignment: WrapAlignment.center,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              _Chip(
                text: nameplate,
                background: AppColors.accent,
                foreground: Colors.white,
              ),
              for (final word in words)
                _Chip(
                  text: word,
                  background: Colors.white,
                  foreground: AppColors.ink,
                ),
            ],
          ),
        ],
      ),
    );
  }
}

class _Chip extends StatelessWidget {
  const _Chip({
    required this.text,
    required this.background,
    required this.foreground,
  });

  final String text;
  final Color background;
  final Color foreground;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.symmetric(
          horizontal: AppSpacing.md, vertical: AppSpacing.sm + 2),
      decoration: BoxDecoration(
        color: background,
        borderRadius: BorderRadius.circular(AppRadius.md),
        border: background == Colors.white
            ? Border.all(color: AppColors.hairline)
            : null,
      ),
      child: Text(
        text,
        style: AppType.codeChip.copyWith(color: foreground),
      ),
    );
  }
}

class _TransferringBody extends StatelessWidget {
  const _TransferringBody({
    required this.received,
    required this.total,
    required this.onCancel,
  });

  final BigInt? received;
  final BigInt? total;
  final VoidCallback onCancel;

  @override
  Widget build(BuildContext context) {
    final receivedValue = received ?? BigInt.zero;
    final totalValue = total ?? BigInt.zero;
    final progress = totalValue == BigInt.zero
        ? null
        : (receivedValue / totalValue).clamp(0.0, 1.0).toDouble();
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const Text(
          '正在发送...',
          textAlign: TextAlign.center,
          style: TextStyle(
              fontSize: 18, fontWeight: FontWeight.w600, color: AppColors.ink),
        ),
        const SizedBox(height: AppSpacing.lg),
        LinearProgressIndicator(value: progress, minHeight: 8, borderRadius: BorderRadius.circular(4)),
        const SizedBox(height: AppSpacing.md),
        Text(
          totalValue == BigInt.zero
              ? '连接中...'
              : '${humanBytes(receivedValue)} / ${humanBytes(totalValue)}'
                  ' (${(progress! * 100).round()}%)',
          textAlign: TextAlign.center,
          style: const TextStyle(
              fontFamily: AppType.mono, fontSize: 13, color: Color(0xFF57534E)),
        ),
        const SizedBox(height: AppSpacing.lg),
        TextButton(
          onPressed: onCancel,
          child: const Text('取消发送', style: TextStyle(color: Color(0xFFB3261E))),
        ),
      ],
    );
  }
}
