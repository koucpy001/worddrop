// Pairing-code parsing and human-readable byte formatting.
//
// A pairing code is `nameplate-word-word-word` (e.g. 7-correct-horse-battery).
// The nameplate is a 1..=9999 canonical decimal; the words are lowercase
// letters (PGP wordlist). Parsing is purely structural on the client: the
// words stay on the device and only the nameplate ever reaches the
// rendezvous (Oracle F1 — the bridge enforces the split; this validator is
// the UI's input gate).

/// One parsed pairing code.
class PairingCode {
  const PairingCode({required this.nameplate, required this.words});

  final String nameplate;
  final List<String> words;

  static final RegExp _pattern =
      RegExp(r'^([1-9]\d{0,3})-([a-z]+)-([a-z]+)-([a-z]+)$');

  /// Parses `raw`, normalizing whitespace + case first. Returns null when the
  /// input is not a valid code (rejects leading zeros, >9999, bad words).
  static PairingCode? tryParse(String raw) {
    final match = _pattern.firstMatch(raw.trim().toLowerCase());
    if (match == null) return null;
    return PairingCode(
      nameplate: match.group(1)!,
      words: [match.group(2)!, match.group(3)!, match.group(4)!],
    );
  }

  /// Canonical display form `nameplate-word-word-word`.
  String get display => '$nameplate-${words.join('-')}';

  /// Whether the four parts are visually distinct — structural UX detail:
  /// the receiver's typed code and the sender's shown code must match.
  @override
  String toString() => display;
}

/// Formats a byte count for Chinese UI copy (binary units, one decimal):
/// 512 B / 12.5 KiB / 1.2 MiB / 3.4 GiB.
String humanBytes(BigInt bytes) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  var value = bytes.toDouble();
  var unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  if (unit == 0) return '${bytes.toInt()} ${units[unit]}';
  return '${value.toStringAsFixed(1)} ${units[unit]}';
}
