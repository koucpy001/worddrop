// Offer dialog — the receiver's two-sided confirm before any file lands
// (LocalSend-style: file names + sizes + total, 接收 / 拒绝 buttons).
// Default-deny: nothing is accepted unless the user taps 接收.

import 'package:flutter/material.dart';

import 'package:worddrop/src/rust/api/session.dart' show FileMetaDto, OfferDto;
import 'package:worddrop/theme.dart';
import 'package:worddrop/util/format.dart';

/// Returns `true` when the user accepted the offer.
Future<bool> showOfferDialog(BuildContext context, OfferDto offer) async {
  final accepted = await showDialog<bool>(
    context: context,
    barrierDismissible: false,
    builder: (_) => OfferDialog(offer: offer),
  );
  return accepted ?? false;
}

class OfferDialog extends StatelessWidget {
  const OfferDialog({super.key, required this.offer});

  final OfferDto offer;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return AlertDialog(
      title: const Text('接收传输？'),
      content: SizedBox(
        width: 360,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              '发送方想向你发送 ${offer.files.length} 个文件，共 '
              '${humanBytes(offer.totalBytes)}',
              style: const TextStyle(fontSize: 14, color: AppColors.ink),
            ),
            const SizedBox(height: AppSpacing.md),
            Flexible(
              child: ConstrainedBox(
                constraints: const BoxConstraints(maxHeight: 220),
                child: ListView.separated(
                  shrinkWrap: true,
                  itemCount: offer.files.length,
                  separatorBuilder: (_, _) =>
                      const Divider(height: 1, color: AppColors.hairline),
                  itemBuilder: (context, index) {
                    final file = offer.files[index];
                    return _FileRow(file: file);
                  },
                ),
              ),
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('拒绝'),
        ),
        FilledButton(
          style: FilledButton.styleFrom(
            backgroundColor: scheme.primary,
            foregroundColor: scheme.onPrimary,
          ),
          onPressed: () => Navigator.of(context).pop(true),
          child: const Text('接收'),
        ),
      ],
    );
  }
}

class _FileRow extends StatelessWidget {
  const _FileRow({required this.file});

  final FileMetaDto file;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: AppSpacing.sm + 2),
      child: Row(
        children: [
          const Icon(Icons.insert_drive_file_outlined,
              size: 20, color: Color(0xFF78716C)),
          const SizedBox(width: AppSpacing.sm + 2),
          Expanded(
            child: Text(
              file.name,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(
                  fontSize: 14, fontWeight: FontWeight.w500, color: AppColors.ink),
            ),
          ),
          const SizedBox(width: AppSpacing.sm),
          Text(
            humanBytes(file.size),
            style: const TextStyle(
                fontFamily: AppType.mono, fontSize: 13, color: Color(0xFF78716C)),
          ),
        ],
      ),
    );
  }
}
