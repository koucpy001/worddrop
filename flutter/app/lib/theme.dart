// Design tokens for the worddrop Flutter app.
//
// One seed (deep evergreen — croc skin) drives the Material 3 color scheme;
// the warm sand scaffold + amber accent are the app's own surface choices.
// Spacing is an 8px grid; all radii are the four step values below.

import 'package:flutter/material.dart';

/// Brand palette. Prefer `Theme.of(context).colorScheme` for widget colors;
/// use these for the app's distinctive surfaces.
abstract final class AppColors {
  /// Deep evergreen seed — the brand color (croc).
  static const Color seed = Color(0xFF14532D);

  /// Warm sand app background.
  static const Color sand = Color(0xFFF6F4EE);

  /// Ink for primary text on light surfaces.
  static const Color ink = Color(0xFF1C1917);

  /// Light green tint behind the pairing-code card.
  static const Color codeBg = Color(0xFFEDF3EA);

  /// Amber accent — the nameplate chip (distinct from the word chips).
  static const Color accent = Color(0xFFD97706);

  /// Hairline borders on cards.
  static const Color hairline = Color(0xFFE3E0D6);
}

/// 8px-grid spacing scale.
abstract final class AppSpacing {
  static const double xs = 4;
  static const double sm = 8;
  static const double md = 16;
  static const double lg = 24;
  static const double xl = 32;
  static const double xxl = 48;
}

/// Corner radii.
abstract final class AppRadius {
  static const double sm = 8;
  static const double md = 12;
  static const double lg = 16;
}

/// Monospace style for pairing codes and byte counts.
abstract final class AppType {
  static const String mono = 'monospace';

  /// The large pairing-code text on the send screen.
  static const TextStyle codeLarge = TextStyle(
    fontFamily: mono,
    fontSize: 22,
    fontWeight: FontWeight.w600,
    letterSpacing: 1.2,
    height: 1.3,
    color: AppColors.ink,
  );

  /// Code text inside a chip.
  static const TextStyle codeChip = TextStyle(
    fontFamily: mono,
    fontSize: 16,
    fontWeight: FontWeight.w600,
    letterSpacing: 0.8,
  );

  /// Input field code text.
  static const TextStyle codeInput = TextStyle(
    fontFamily: mono,
    fontSize: 16,
    letterSpacing: 0.5,
  );
}

/// Builds the app theme from the tokens above.
ThemeData buildAppTheme() {
  final scheme = ColorScheme.fromSeed(seedColor: AppColors.seed);
  return ThemeData(
    colorScheme: scheme,
    scaffoldBackgroundColor: AppColors.sand,
    appBarTheme: const AppBarTheme(
      backgroundColor: AppColors.sand,
      foregroundColor: AppColors.ink,
      elevation: 0,
      scrolledUnderElevation: 0,
      centerTitle: true,
    ),
    cardTheme: CardThemeData(
      color: Colors.white,
      elevation: 0,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(AppRadius.lg),
        side: const BorderSide(color: AppColors.hairline),
      ),
    ),
    filledButtonTheme: FilledButtonThemeData(
      style: FilledButton.styleFrom(
        minimumSize: const Size(64, 48),
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(AppRadius.md),
        ),
        textStyle: const TextStyle(fontSize: 16, fontWeight: FontWeight.w600),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        minimumSize: const Size(64, 48),
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(AppRadius.md),
        ),
        textStyle: const TextStyle(fontSize: 16, fontWeight: FontWeight.w600),
      ),
    ),
    textButtonTheme: TextButtonThemeData(
      style: TextButton.styleFrom(
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(AppRadius.md),
        ),
        textStyle: const TextStyle(fontSize: 16),
      ),
    ),
    inputDecorationTheme: InputDecorationTheme(
      filled: true,
      fillColor: Colors.white,
      contentPadding:
          const EdgeInsets.symmetric(horizontal: AppSpacing.md, vertical: 14),
      border: OutlineInputBorder(
        borderRadius: BorderRadius.circular(AppRadius.md),
        borderSide: const BorderSide(color: AppColors.hairline),
      ),
      enabledBorder: OutlineInputBorder(
        borderRadius: BorderRadius.circular(AppRadius.md),
        borderSide: const BorderSide(color: AppColors.hairline),
      ),
      focusedBorder: OutlineInputBorder(
        borderRadius: BorderRadius.circular(AppRadius.md),
        borderSide: BorderSide(color: scheme.primary, width: 1.6),
      ),
    ),
    snackBarTheme: SnackBarThemeData(
      behavior: SnackBarBehavior.floating,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(AppRadius.md),
      ),
    ),
    dialogTheme: DialogThemeData(
      backgroundColor: Colors.white,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(AppRadius.lg),
      ),
    ),
  );
}
