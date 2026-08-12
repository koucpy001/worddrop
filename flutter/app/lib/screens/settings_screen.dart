// Settings screen — rendezvous URL, relay URL, data dir, overwrite toggle.
// Loads config via the bridge (getConfig) and saves individual keys via
// setConfig. Constructors accept injectable callbacks so widget tests can
// supply fakes (hermetic flutter test, no native cdylib).
//
// Chinese labels per AGENTS.md: 中继服务器地址, 配对服务器地址, 数据目录, 覆盖已有文件.

import 'package:flutter/material.dart';

import 'package:app/src/rust/api/config.dart' as bridge;
import 'package:app/theme.dart';

/// Injectable config callbacks so widget tests stay hermetic.
typedef GetConfigFn = Future<bridge.ConfigDto> Function();
typedef SetConfigFn = Future<String> Function(String key, String value);

/// Default helpers that call the real FRB bridge.
Future<bridge.ConfigDto> _liveGetConfig() => bridge.getConfig();
Future<String> _liveSetConfig(String key, String value) =>
    bridge.setConfig(key: key, value: value);

class SettingsScreen extends StatefulWidget {
  const SettingsScreen({
    super.key,
    this.getConfig = _liveGetConfig,
    this.setConfig = _liveSetConfig,
  });

  /// Load config (defaults to the live bridge).
  final GetConfigFn getConfig;

  /// Save one config key (defaults to the live bridge).
  final SetConfigFn setConfig;

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  final _rendezvousController = TextEditingController();
  final _relayController = TextEditingController();
  final _dataDirController = TextEditingController();
  bool _overwrite = false;
  bool _loading = true;
  String? _error;

  @override
  void initState() {
    super.initState();
    // React to edits so the official-server hint appears/disappears as soon
    // as the URL fields leave (or return to) the local defaults.
    _rendezvousController.addListener(_onUrlFieldChanged);
    _relayController.addListener(_onUrlFieldChanged);
    _load();
  }

  @override
  void dispose() {
    _rendezvousController.removeListener(_onUrlFieldChanged);
    _relayController.removeListener(_onUrlFieldChanged);
    _rendezvousController.dispose();
    _relayController.dispose();
    _dataDirController.dispose();
    super.dispose();
  }

  void _onUrlFieldChanged() {
    if (mounted) setState(() {});
  }

  /// True while both server fields still show the built-in local defaults
  /// (127.0.0.1) — i.e. the user has not configured public servers yet.
  bool get _showingDefaultServers {
    return _rendezvousController.text.contains('127.0.0.1') &&
        _relayController.text.contains('127.0.0.1');
  }

  Future<void> _load() async {
    try {
      final config = await widget.getConfig();
      if (!mounted) return;
      setState(() {
        _rendezvousController.text = config.rendezvousUrl;
        _relayController.text = config.relayUrl;
        _dataDirController.text = config.dataDir;
        _overwrite = config.overwrite;
        _loading = false;
        _error = null;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _loading = false;
        _error = '加载设置失败: $e';
      });
    }
  }

  Future<void> _save(String key, String value) async {
    try {
      await widget.setConfig(key, value);
      if (!mounted) return;
      ScaffoldMessenger.of(context)
        ..hideCurrentSnackBar()
        ..showSnackBar(const SnackBar(
          content: Text('已保存'),
          duration: Duration(seconds: 2),
        ));
    } catch (e) {
      if (!mounted) return;
      ScaffoldMessenger.of(context)
        ..hideCurrentSnackBar()
        ..showSnackBar(SnackBar(content: Text('保存失败: $e')));
    }
  }

  Future<void> _saveOverwrite(bool value) async {
    setState(() => _overwrite = value);
    await _save('overwrite', value ? 'true' : 'false');
  }

  @override
  Widget build(BuildContext context) {
    if (_loading) {
      return const Center(child: CircularProgressIndicator());
    }

    if (_error != null) {
      return Center(
        child: Padding(
          padding: const EdgeInsets.all(AppSpacing.lg),
          child: Column(
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              Text(_error!,
                  style: const TextStyle(color: Color(0xFFB3261E))),
              const SizedBox(height: AppSpacing.md),
              FilledButton(onPressed: _load, child: const Text('重试')),
            ],
          ),
        ),
      );
    }

    return ListView(
      padding: const EdgeInsets.all(AppSpacing.md),
      children: [
        _SectionLabel('网络设置'),
        _UrlField(
          controller: _rendezvousController,
          label: '配对服务器地址',
          hint: 'http://127.0.0.1:8080',
          icon: Icons.link_outlined,
          onSaved: (v) => _save('rendezvous_url', v),
        ),
        const SizedBox(height: AppSpacing.md),
        _UrlField(
          controller: _relayController,
          label: '中继服务器地址',
          hint: 'http://127.0.0.1:3340',
          icon: Icons.cloud_outlined,
          onSaved: (v) => _save('relay_url', v),
        ),
        if (_showingDefaultServers) ...[
          const SizedBox(height: AppSpacing.md),
          const _OfficialServerHint(),
        ],
        const SizedBox(height: AppSpacing.lg),
        _SectionLabel('存储设置'),
        _UrlField(
          controller: _dataDirController,
          label: '数据目录',
          hint: '/home/user/.config/worddrop',
          icon: Icons.folder_outlined,
          onSaved: (v) => _save('data_dir', v),
        ),
        const SizedBox(height: AppSpacing.md),
        _ToggleTile(
          icon: Icons.file_copy_outlined,
          label: '覆盖已有文件',
          subtitle: '接收文件时，如果目标已存在则直接覆盖',
          value: _overwrite,
          onChanged: _saveOverwrite,
        ),
        const SizedBox(height: AppSpacing.xl),
        _ResetButton(onReset: _load),
      ],
    );
  }
}

/// One-line helper shown while the server fields still carry the local
/// defaults: points the user at the official public servers. Pure hint — the
/// default values themselves are NOT changed (Bug 4 option B).
class _OfficialServerHint extends StatelessWidget {
  const _OfficialServerHint();

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.all(AppSpacing.sm + 2),
      decoration: BoxDecoration(
        color: const Color(0xFFF1F5F1),
        borderRadius: BorderRadius.circular(AppRadius.md),
        border: Border.all(color: const Color(0xFFD6E2D0)),
      ),
      child: const Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(Icons.info_outline, size: 16, color: Color(0xFF57534E)),
          SizedBox(width: AppSpacing.sm),
          Expanded(
            child: Text(
              '官方服务：https://relay.worddrop.cloud / https://pair.worddrop.cloud（自托管或局域网可留空使用默认）',
              style: TextStyle(
                  fontSize: 12.5, color: Color(0xFF57534E), height: 1.5),
            ),
          ),
        ],
      ),
    );
  }
}

class _SectionLabel extends StatelessWidget {
  const _SectionLabel(this.label);

  final String label;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(
          AppSpacing.sm, AppSpacing.sm, AppSpacing.sm, AppSpacing.sm),
      child: Text(label,
          style: const TextStyle(
              fontSize: 13,
              fontWeight: FontWeight.w600,
              color: Color(0xFF78716C),
              letterSpacing: 0.3)),
    );
  }
}

class _UrlField extends StatefulWidget {
  const _UrlField({
    required this.controller,
    required this.label,
    required this.hint,
    required this.icon,
    required this.onSaved,
  });

  final TextEditingController controller;
  final String label;
  final String hint;
  final IconData icon;
  final ValueChanged<String> onSaved;

  @override
  State<_UrlField> createState() => _UrlFieldState();
}

class _UrlFieldState extends State<_UrlField> {
  bool _dirty = false;

  @override
  Widget build(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(AppSpacing.md),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(widget.icon,
                    size: 18, color: const Color(0xFF57534E)),
                const SizedBox(width: AppSpacing.sm),
                Text(widget.label,
                    style: const TextStyle(
                        fontSize: 14,
                        fontWeight: FontWeight.w500,
                        color: AppColors.ink)),
              ],
            ),
            const SizedBox(height: AppSpacing.sm),
            TextField(
              controller: widget.controller,
              style: const TextStyle(fontSize: 14, fontFamily: AppType.mono),
              decoration: InputDecoration(
                hintText: widget.hint,
                hintStyle: const TextStyle(
                    fontSize: 13, color: Color(0xFFA8A29E)),
                contentPadding: const EdgeInsets.symmetric(
                    horizontal: AppSpacing.sm + 2, vertical: 10),
                suffixIcon: _dirty
                    ? IconButton(
                        icon: const Icon(Icons.check, size: 20),
                        onPressed: () {
                          widget.onSaved(widget.controller.text);
                          setState(() => _dirty = false);
                        },
                      )
                    : null,
              ),
              onChanged: (_) {
                if (!_dirty) setState(() => _dirty = true);
              },
              onSubmitted: (v) {
                widget.onSaved(v);
                setState(() => _dirty = false);
              },
            ),
          ],
        ),
      ),
    );
  }
}

class _ToggleTile extends StatelessWidget {
  const _ToggleTile({
    required this.icon,
    required this.label,
    required this.subtitle,
    required this.value,
    required this.onChanged,
  });

  final IconData icon;
  final String label;
  final String subtitle;
  final bool value;
  final ValueChanged<bool> onChanged;

  @override
  Widget build(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.symmetric(
            horizontal: AppSpacing.md, vertical: AppSpacing.sm),
        child: Row(
          children: [
            Icon(icon, size: 18, color: const Color(0xFF57534E)),
            const SizedBox(width: AppSpacing.sm),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(label,
                      style: const TextStyle(
                          fontSize: 14,
                          fontWeight: FontWeight.w500,
                          color: AppColors.ink)),
                  Text(subtitle,
                      style: const TextStyle(
                          fontSize: 12, color: Color(0xFFA8A29E))),
                ],
              ),
            ),
            Switch.adaptive(value: value, onChanged: onChanged),
          ],
        ),
      ),
    );
  }
}

class _ResetButton extends StatelessWidget {
  const _ResetButton({required this.onReset});

  final VoidCallback onReset;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: TextButton.icon(
        onPressed: onReset,
        icon: const Icon(Icons.refresh_outlined, size: 18),
        label: const Text('重新加载设置'),
        style: TextButton.styleFrom(
            foregroundColor: const Color(0xFF78716C)),
      ),
    );
  }
}
