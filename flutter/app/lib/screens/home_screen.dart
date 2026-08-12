// Home screen — bottom navigation between the pairing hub (发送文件/接收文件),
// transfer list, and settings. The send/receive screens push via Navigator so
// the bottom bar stays visible only on the top-level tabs.
//
// Navigation tabs: 0=传输(home), 1=传输列表(transfers), 2=设置(settings).

import 'package:flutter/material.dart';

import 'package:app/backend/pairing_backend.dart';
import 'package:app/screens/receive_screen.dart';
import 'package:app/screens/send_screen.dart';
import 'package:app/screens/settings_screen.dart';
import 'package:app/screens/transfers_screen.dart';
import 'package:app/theme.dart';

class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key, required this.backendFactory});

  final SessionBackendFactory backendFactory;

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  int _navIndex = 0;

  // Tab 0 keeps no AppBar title — the body has the logo and tagline.
  static const _titles = ['', '传输列表', '设置'];

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: Text(_titles[_navIndex])),
      body: switch (_navIndex) {
        0 => _HomeBody(backendFactory: widget.backendFactory),
        1 => const TransfersScreen(),
        2 => const SettingsScreen(),
        _ => _HomeBody(backendFactory: widget.backendFactory),
      },
      bottomNavigationBar: NavigationBar(
        selectedIndex: _navIndex,
        onDestinationSelected: (i) => setState(() => _navIndex = i),
        destinations: const [
          NavigationDestination(
            icon: Icon(Icons.swap_horiz_outlined),
            selectedIcon: Icon(Icons.swap_horiz),
            label: '传输',
          ),
          NavigationDestination(
            icon: Icon(Icons.history_outlined),
            selectedIcon: Icon(Icons.history),
            label: '传输列表',
          ),
          NavigationDestination(
            icon: Icon(Icons.settings_outlined),
            selectedIcon: Icon(Icons.settings),
            label: '设置',
          ),
        ],
      ),
    );
  }
}

/// The original home body (发送文件 / 接收文件 entries), extracted from the
/// old StatelessWidget HomeScreen so it can sit behind the bottom nav.
class _HomeBody extends StatelessWidget {
  const _HomeBody({required this.backendFactory});

  final SessionBackendFactory backendFactory;

  @override
  Widget build(BuildContext context) {
    return SafeArea(
      child: SingleChildScrollView(
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 440),
            child: Padding(
              padding: const EdgeInsets.all(AppSpacing.xl),
              child: Column(
                mainAxisAlignment: MainAxisAlignment.center,
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                Icon(
                  Icons.bolt,
                  size: 56,
                  color: Theme.of(context).colorScheme.primary,
                ),
                const SizedBox(height: AppSpacing.md),
                const Text(
                  'WordDrop',
                  textAlign: TextAlign.center,
                  style: TextStyle(
                    fontSize: 34,
                    fontWeight: FontWeight.w700,
                    letterSpacing: 0.5,
                    color: AppColors.ink,
                  ),
                ),
                const SizedBox(height: AppSpacing.sm),
                const Text(
                  '跨网络安全传输文件',
                  textAlign: TextAlign.center,
                  style: TextStyle(fontSize: 15, color: Color(0xFF57534E)),
                ),
                const SizedBox(height: AppSpacing.xl),
                _EntryCard(
                  icon: Icons.upload_file_outlined,
                  title: '发送文件',
                  subtitle: '选择文件，生成配对码',
                  onTap: () => Navigator.of(context).push(
                    MaterialPageRoute<void>(
                      builder: (_) =>
                          SendScreen(backendFactory: backendFactory),
                    ),
                  ),
                ),
                const SizedBox(height: AppSpacing.md),
                _EntryCard(
                  icon: Icons.download_outlined,
                  title: '接收文件',
                  subtitle: '输入对方的配对码',
                  onTap: () => Navigator.of(context).push(
                    MaterialPageRoute<void>(
                      builder: (_) =>
                          ReceiveScreen(backendFactory: backendFactory),
                    ),
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
      ),
    );
  }
}

class _EntryCard extends StatelessWidget {
  const _EntryCard({
    required this.icon,
    required this.title,
    required this.subtitle,
    required this.onTap,
  });

  final IconData icon;
  final String title;
  final String subtitle;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Card(
      child: InkWell(
        borderRadius: BorderRadius.circular(AppRadius.lg),
        onTap: onTap,
        child: Padding(
          padding: const EdgeInsets.all(AppSpacing.lg),
          child: Row(
            children: [
              Container(
                width: 48,
                height: 48,
                decoration: BoxDecoration(
                  color: scheme.primaryContainer,
                  borderRadius: BorderRadius.circular(AppRadius.md),
                ),
                child: Icon(icon, color: scheme.onPrimaryContainer),
              ),
              const SizedBox(width: AppSpacing.md),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      title,
                      style: const TextStyle(
                        fontSize: 17,
                        fontWeight: FontWeight.w600,
                        color: AppColors.ink,
                      ),
                    ),
                    const SizedBox(height: 2),
                    Text(
                      subtitle,
                      style: const TextStyle(
                          fontSize: 13, color: Color(0xFF78716C)),
                    ),
                  ],
                ),
              ),
              const Icon(Icons.chevron_right, color: Color(0xFFA8A29E)),
            ],
          ),
        ),
      ),
    );
  }
}
