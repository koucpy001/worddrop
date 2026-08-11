// Transfers screen — list of active transfers (progress bar + cancel) and
// completed/cancelled/failed history. History is persisted locally via
// TransferHistory (shared_preferences JSON list of {nameplate, names,
// bytes, time, status}). The pairing word-code is never persisted — history
// shows only the numeric nameplate; the full code appears solely on the
// live (in-memory) active card while a transfer is in flight. All
// user-facing copy is Chinese (AGENTS.md).

import 'package:flutter/material.dart';

import 'package:app/services/transfer_history.dart';
import 'package:app/theme.dart';
import 'package:app/util/format.dart';

class TransfersScreen extends StatefulWidget {
  const TransfersScreen({super.key});

  @override
  State<TransfersScreen> createState() => _TransfersScreenState();
}

class _TransfersScreenState extends State<TransfersScreen> {
  final TransferHistory _store = TransferHistory.instance;

  @override
  void initState() {
    super.initState();
    _store.load();
    _store.addListener(_onChange);
  }

  @override
  void dispose() {
    _store.removeListener(_onChange);
    super.dispose();
  }

  void _onChange() {
    if (mounted) setState(() {});
  }

  Future<void> _cancelActive(ActiveTransfer transfer) async {
    // Call the session's cancel callback if registered.
    await transfer.onCancel?.call();
    await _store.cancelTransfer(transfer.code);
  }

  @override
  Widget build(BuildContext context) {
    final active = _store.active;
    final history = _store.history;

    if (active.isEmpty && history.isEmpty) {
      return const _EmptyBody();
    }

    return ListView(
      padding: const EdgeInsets.symmetric(
        horizontal: AppSpacing.md,
        vertical: AppSpacing.sm,
      ),
      children: [
        if (active.isNotEmpty) ...[
          const Padding(
            padding: EdgeInsets.fromLTRB(
                AppSpacing.sm, AppSpacing.md, AppSpacing.sm, AppSpacing.sm),
            child: Text('进行中',
                style: TextStyle(
                    fontSize: 14,
                    fontWeight: FontWeight.w600,
                    color: AppColors.ink)),
          ),
          ...active.map((t) => _ActiveCard(
                transfer: t,
                onCancel: () => _cancelActive(t),
              )),
        ],
        if (history.isNotEmpty) ...[
          Padding(
            padding: const EdgeInsets.fromLTRB(
                AppSpacing.sm, AppSpacing.md, AppSpacing.sm, AppSpacing.sm),
            child: Text(
              active.isEmpty ? '全部记录' : '传输记录',
              style: const TextStyle(
                  fontSize: 14,
                  fontWeight: FontWeight.w600,
                  color: AppColors.ink),
            ),
          ),
          ...history.map((r) => _HistoryCard(record: r)),
        ],
        const SizedBox(height: AppSpacing.lg),
      ],
    );
  }
}

class _EmptyBody extends StatelessWidget {
  const _EmptyBody();

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Column(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          Icon(Icons.history_outlined,
              size: 64,
              color: Theme.of(context)
                  .colorScheme
                  .primary
                  .withValues(alpha: 0.6)),
          const SizedBox(height: AppSpacing.md),
          const Text(
            '暂无传输记录',
            style: TextStyle(
                fontSize: 16,
                fontWeight: FontWeight.w500,
                color: Color(0xFF78716C)),
          ),
          const SizedBox(height: AppSpacing.sm),
          const Text(
            '完成一次传输后，记录将显示在这里',
            style: TextStyle(fontSize: 13.5, color: Color(0xFFA8A29E)),
          ),
        ],
      ),
    );
  }
}

class _ActiveCard extends StatelessWidget {
  const _ActiveCard({required this.transfer, required this.onCancel});

  final ActiveTransfer transfer;
  final VoidCallback onCancel;

  @override
  Widget build(BuildContext context) {
    final progress = transfer.progress;
    return Card(
      margin: const EdgeInsets.symmetric(vertical: AppSpacing.xs),
      child: Padding(
        padding: const EdgeInsets.all(AppSpacing.md),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                _DirectionIcon(direction: transfer.direction),
                const SizedBox(width: AppSpacing.sm),
                Expanded(
                  child: Text(
                    transfer.names.join('、'),
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: const TextStyle(
                        fontSize: 14, fontWeight: FontWeight.w600),
                  ),
                ),
                const SizedBox(width: AppSpacing.sm),
                _StatusChip(label: transfer.statusLabel, active: true),
              ],
            ),
            const SizedBox(height: AppSpacing.sm),
            Row(
              children: [
                // Full code shown while live: the user reads it to share
                // with the peer. It exists only in memory here — history
                // persists just the numeric nameplate.
                Text(
                  transfer.code,
                  style: const TextStyle(
                      fontFamily: AppType.mono,
                      fontSize: 12,
                      color: Color(0xFF78716C)),
                ),
                const Spacer(),
                Text(
                  humanBytes(transfer.totalBytes),
                  style: const TextStyle(
                      fontSize: 13, color: Color(0xFF57534E)),
                ),
              ],
            ),
            const SizedBox(height: AppSpacing.sm),
            LinearProgressIndicator(
              value: progress,
              minHeight: 6,
              borderRadius: BorderRadius.circular(3),
            ),
            const SizedBox(height: AppSpacing.sm),
            Row(
              mainAxisAlignment: MainAxisAlignment.spaceBetween,
              children: [
                if (transfer.received > BigInt.zero)
                  Text(
                    '${humanBytes(transfer.received)} / ${humanBytes(transfer.totalBytes)}${progress != null ? " (${(progress * 100).round()}%)" : ""}',
                    style: const TextStyle(
                        fontFamily: AppType.mono,
                        fontSize: 11,
                        color: Color(0xFF78716C)),
                  )
                else
                  const SizedBox.shrink(),
                TextButton(
                  onPressed: onCancel,
                  style: TextButton.styleFrom(
                    foregroundColor: const Color(0xFFB3261E),
                    minimumSize: const Size(0, 32),
                    padding:
                        const EdgeInsets.symmetric(horizontal: AppSpacing.sm),
                    tapTargetSize: MaterialTapTargetSize.shrinkWrap,
                  ),
                  child: const Text('取消', style: TextStyle(fontSize: 13)),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}

class _HistoryCard extends StatelessWidget {
  const _HistoryCard({required this.record});

  final TransferRecord record;

  @override
  Widget build(BuildContext context) {
    final (String label, Color color) = switch (record.status) {
      'completed' => ('已完成', const Color(0xFF14532D)),
      'cancelled' => ('已取消', const Color(0xFF57534E)),
      'failed' => ('失败', const Color(0xFFB3261E)),
      _ => ('未知', const Color(0xFF78716C)),
    };

    return Card(
      margin: const EdgeInsets.symmetric(vertical: AppSpacing.xs),
      child: Padding(
        padding: const EdgeInsets.all(AppSpacing.md),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                _DirectionIcon(direction: record.direction),
                const SizedBox(width: AppSpacing.sm),
                Expanded(
                  child: Text(
                    record.names.join('、'),
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: const TextStyle(
                        fontSize: 14, fontWeight: FontWeight.w600),
                  ),
                ),
                const SizedBox(width: AppSpacing.sm),
                _StatusChip(label: label, active: false, color: color),
              ],
            ),
            const SizedBox(height: AppSpacing.sm),
            Row(
              children: [
                Text(
                  record.nameplate,
                  style: const TextStyle(
                      fontFamily: AppType.mono,
                      fontSize: 12,
                      color: Color(0xFF78716C)),
                ),
                const Spacer(),
                Text(
                  humanBytes(record.bytes),
                  style: const TextStyle(
                      fontSize: 13, color: Color(0xFF57534E)),
                ),
              ],
            ),
            const SizedBox(height: AppSpacing.xs),
            Text(
              _formatTime(record.time),
              style: const TextStyle(fontSize: 11, color: Color(0xFFA8A29E)),
            ),
          ],
        ),
      ),
    );
  }

  String _formatTime(DateTime t) {
    final now = DateTime.now();
    final diff = now.difference(t);
    if (diff.inMinutes < 1) return '刚刚';
    if (diff.inMinutes < 60) return '${diff.inMinutes} 分钟前';
    if (diff.inHours < 24) return '${diff.inHours} 小时前';
    return '${t.month}/${t.day} ${t.hour.toString().padLeft(2, '0')}:${t.minute.toString().padLeft(2, '0')}';
  }
}

class _DirectionIcon extends StatelessWidget {
  const _DirectionIcon({required this.direction});

  final String direction;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Icon(
      direction == 'received' ? Icons.download_outlined : Icons.upload_outlined,
      size: 18,
      color: direction == 'received'
          ? scheme.primary
          : const Color(0xFFD97706),
    );
  }
}

class _StatusChip extends StatelessWidget {
  const _StatusChip({
    required this.label,
    required this.active,
    this.color,
  });

  final String label;
  final bool active;
  final Color? color;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final Color bg;
    final Color fg;

    if (active) {
      bg = scheme.primaryContainer;
      fg = scheme.onPrimaryContainer;
    } else {
      fg = color ?? const Color(0xFF57534E);
      bg = fg.withValues(alpha: 0.10);
    }

    return Container(
      padding: const EdgeInsets.symmetric(
          horizontal: AppSpacing.sm + 2, vertical: 2),
      decoration: BoxDecoration(
        color: bg,
        borderRadius: BorderRadius.circular(AppRadius.sm),
      ),
      child: Text(label,
          style: TextStyle(fontSize: 11, fontWeight: FontWeight.w600, color: fg)),
    );
  }
}
