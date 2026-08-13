// Status/error banner — the app's single surface for flow state lines
// (info, success, error, cancelled). All copy is Chinese (AGENTS.md).

import 'package:flutter/material.dart';

import 'package:worddrop/theme.dart';

enum BannerVariant { info, success, error, cancelled }

class StatusBanner extends StatelessWidget {
  const StatusBanner({
    super.key,
    required this.variant,
    required this.message,
    this.onAction,
    this.actionLabel,
  });

  final BannerVariant variant;
  final String message;
  final VoidCallback? onAction;
  final String? actionLabel;

  @override
  Widget build(BuildContext context) {
    final (Color background, Color foreground, IconData icon) =
        switch (variant) {
      BannerVariant.info => (const Color(0xFFE7F0FD), const Color(0xFF1D4ED8), Icons.info_outline),
      BannerVariant.success => (const Color(0xFFE3F3E6), const Color(0xFF14532D), Icons.check_circle_outline),
      BannerVariant.error => (const Color(0xFFFDE7E5), const Color(0xFFB3261E), Icons.error_outline),
      BannerVariant.cancelled => (const Color(0xFFEFEEEA), const Color(0xFF57534E), Icons.cancel_outlined),
    };

    return Container(
      width: double.infinity,
      padding: const EdgeInsets.symmetric(
        horizontal: AppSpacing.md,
        vertical: AppSpacing.md,
      ),
      decoration: BoxDecoration(
        color: background,
        borderRadius: BorderRadius.circular(AppRadius.md),
      ),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(icon, color: foreground, size: 22),
          const SizedBox(width: AppSpacing.sm + 2),
          Expanded(
            child: Text(
              message,
              style: TextStyle(
                color: foreground,
                fontSize: 14,
                fontWeight: FontWeight.w500,
                height: 1.4,
              ),
            ),
          ),
          if (onAction != null && actionLabel != null) ...[
            const SizedBox(width: AppSpacing.sm),
            TextButton(
              onPressed: onAction,
              style: TextButton.styleFrom(
                foregroundColor: foreground,
                minimumSize: const Size(0, 32),
                padding: const EdgeInsets.symmetric(horizontal: AppSpacing.sm),
                tapTargetSize: MaterialTapTargetSize.shrinkWrap,
              ),
              child: Text(actionLabel!),
            ),
          ],
        ],
      ),
    );
  }
}
