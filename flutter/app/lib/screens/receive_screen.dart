// Receive screen: enter the pairing code -> claim the offer -> 接收/拒绝
// dialog -> download progress -> done/error/cancelled banners.

import 'dart:async';

import 'package:flutter/material.dart';

import 'package:app/backend/pairing_backend.dart';
import 'package:app/src/rust/api/events.dart';
import 'package:app/src/rust/api/session.dart' show OfferDto, SessionRole;
import 'package:app/theme.dart';
import 'package:app/util/format.dart';
import 'package:app/widgets/offer_dialog.dart';
import 'package:app/widgets/status_banner.dart';

enum ReceiveStage { input, connecting, transferring, done, failed, cancelled, declined }

class ReceiveScreen extends StatefulWidget {
  const ReceiveScreen({super.key, required this.backendFactory});

  final SessionBackendFactory backendFactory;

  @override
  State<ReceiveScreen> createState() => _ReceiveScreenState();
}

class _ReceiveScreenState extends State<ReceiveScreen> {
  final TextEditingController _codeController = TextEditingController();

  ReceiveStage _stage = ReceiveStage.input;
  SessionBackend? _backend;
  StreamSubscription<BridgeEvent>? _events;
  BigInt? _received;
  BigInt? _total;
  String? _exportingName;
  String? _error;
  String? _formError;
  bool _connecting = false;

  @override
  void dispose() {
    _codeController.dispose();
    // frb streams: never await subscription.cancel() (T16) — drop the
    // session to stop the Rust flow instead.
    _events?.cancel();
    _backend?.disposeSession();
    super.dispose();
  }

  Future<SessionBackend> _ensureBackend() async {
    var backend = _backend;
    if (backend == null) {
      backend = await widget.backendFactory(SessionRole.receiver);
      _backend = backend;
      _events = backend.watchTransfer().listen(_onEvent);
    }
    return backend;
  }

  Future<void> _connect() async {
    final code = PairingCode.tryParse(_codeController.text);
    if (code == null) {
      setState(() => _formError = '配对码格式不正确，应为：数字-单词-单词-单词');
      return;
    }
    setState(() {
      _formError = null;
      _connecting = true;
      _stage = ReceiveStage.connecting;
      _error = null;
    });
    try {
      final backend = await _ensureBackend();
      final offer = await backend.claimCode(code.display);
      if (!mounted) return;
      setState(() {
        _connecting = false;
        _stage = ReceiveStage.connecting; // still awaiting user decision
      });
      await _showOffer(offer);
    } catch (error) {
      if (!mounted) return;
      setState(() {
        _connecting = false;
        _stage = ReceiveStage.failed;
        _error = '配对失败: $error';
      });
    }
  }

  Future<void> _showOffer(OfferDto offer) async {
    final accepted = await showOfferDialog(context, offer);
    if (!mounted) return;
    if (accepted) {
      setState(() => _stage = ReceiveStage.transferring);
      try {
        await _backend?.acceptOffer(); // empty targetDir -> received/ subdir
      } catch (error) {
        if (!mounted) return;
        setState(() {
          _stage = ReceiveStage.failed;
          _error = '接受失败: $error';
        });
      }
    } else {
      try {
        await _backend?.declineOffer('用户拒绝');
      } catch (_) {
        // Event stream carries the outcome; ignore late failures.
      }
      if (!mounted) return;
      setState(() => _stage = ReceiveStage.declined);
    }
  }

  void _onEvent(BridgeEvent event) {
    if (!mounted) return;
    switch (event.kind) {
      case 'downloading':
        setState(() {
          _received = event.received;
          _total = event.total;
        });
      case 'exporting':
        setState(() => _exportingName = event.name);
      case 'done':
        setState(() => _stage = ReceiveStage.done);
      case 'phase':
        if (event.phase == 'cancelled') {
          setState(() => _stage = ReceiveStage.cancelled);
        } else if (event.phase == 'done') {
          setState(() => _stage = ReceiveStage.done);
        }
      case 'error':
        setState(() {
          _stage = ReceiveStage.failed;
          _error = '传输失败: ${event.message ?? '未知错误'}';
        });
      default:
        break;
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('接收文件')),
      body: SafeArea(
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 440),
            child: Padding(
              padding: const EdgeInsets.all(AppSpacing.lg),
              child: switch (_stage) {
                ReceiveStage.input ||
                ReceiveStage.connecting => _buildInput(),
                ReceiveStage.transferring => _buildTransferring(),
                ReceiveStage.done => StatusBanner(
                    variant: BannerVariant.success,
                    message: _exportingName == null
                        ? '传输完成'
                        : '传输完成，已保存到「received」目录',
                    actionLabel: '完成',
                    onAction: () => Navigator.of(context).pop(),
                  ),
                ReceiveStage.failed => StatusBanner(
                    variant: BannerVariant.error,
                    message: _error ?? '传输失败',
                    actionLabel: '返回',
                    onAction: () => Navigator.of(context).pop(),
                  ),
                ReceiveStage.cancelled => StatusBanner(
                    variant: BannerVariant.cancelled,
                    message: '接收已取消',
                    actionLabel: '返回',
                    onAction: () => Navigator.of(context).pop(),
                  ),
                ReceiveStage.declined => StatusBanner(
                    variant: BannerVariant.info,
                    message: '已拒绝传输',
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

  Widget _buildInput() {
    final scheme = Theme.of(context).colorScheme;
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Icon(Icons.keyboard_alt_outlined,
            size: 56, color: scheme.primary.withValues(alpha: 0.7)),
        const SizedBox(height: AppSpacing.md),
        const Text(
          '输入配对码',
          textAlign: TextAlign.center,
          style: TextStyle(
              fontSize: 18, fontWeight: FontWeight.w600, color: AppColors.ink),
        ),
        const SizedBox(height: AppSpacing.sm),
        const Text(
          '向发送方获取配对码，格式为 数字-单词-单词-单词',
          textAlign: TextAlign.center,
          style: TextStyle(fontSize: 13.5, color: Color(0xFF78716C), height: 1.5),
        ),
        const SizedBox(height: AppSpacing.lg),
        TextField(
          controller: _codeController,
          enabled: !_connecting,
          style: AppType.codeInput,
          textAlign: TextAlign.center,
          autocorrect: false,
          enableSuggestions: false,
          decoration: InputDecoration(
            hintText: '例如 7-correct-horse-battery',
            hintStyle: AppType.codeInput.copyWith(color: const Color(0xFFA8A29E)),
            errorText: _formError,
          ),
          onSubmitted: (_) => _connect(),
        ),
        const SizedBox(height: AppSpacing.lg),
        FilledButton(
          onPressed: _connecting ? null : _connect,
          child: _connecting
              ? const SizedBox(
                  width: 20,
                  height: 20,
                  child: CircularProgressIndicator(strokeWidth: 2.5),
                )
              : const Text('连接'),
        ),
      ],
    );
  }

  Widget _buildTransferring() {
    final receivedValue = _received ?? BigInt.zero;
    final totalValue = _total ?? BigInt.zero;
    final progress = totalValue == BigInt.zero
        ? null
        : (receivedValue / totalValue).clamp(0.0, 1.0).toDouble();
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const Text(
          '正在接收...',
          textAlign: TextAlign.center,
          style: TextStyle(
              fontSize: 18, fontWeight: FontWeight.w600, color: AppColors.ink),
        ),
        if (_exportingName != null) ...[
          const SizedBox(height: AppSpacing.sm),
          Text(
            '正在保存 $_exportingName',
            textAlign: TextAlign.center,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: const TextStyle(fontSize: 13, color: Color(0xFF78716C)),
          ),
        ],
        const SizedBox(height: AppSpacing.lg),
        LinearProgressIndicator(
            value: progress,
            minHeight: 8,
            borderRadius: BorderRadius.circular(4)),
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
      ],
    );
  }
}
