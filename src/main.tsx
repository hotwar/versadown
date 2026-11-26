import React from 'react';
import ReactDOM from 'react-dom/client';
import { loadActiveLocale } from './i18n';
import { isLauncherWindow } from './lib/launcherWindow';
import './styles.css';

// Each window loads only its own UI: the launcher renderer never parses the
// manager's code, and vice versa.
async function loadWindowComponent(launcherWindow: boolean): Promise<React.ComponentType> {
  if (new URLSearchParams(window.location.search).get('window') === 'agent-access') return (await import('./components/AgentInteractionWindow')).default;
  if (launcherWindow) return (await import('./components/LauncherWindow')).LauncherWindow;
  return (await import('./App')).App;
}

async function mount(): Promise<void> {
  const launcherWindow = isLauncherWindow();
  if (launcherWindow) document.documentElement.classList.add('launcher-window');
  // A missing catalog chunk falls back to the bundled zh-CN/en text.
  const [Root] = await Promise.all([
    loadWindowComponent(launcherWindow),
    loadActiveLocale().catch((error: unknown) => console.error('Unable to load locale catalog:', error)),
  ]);
  ReactDOM.createRoot(document.getElementById('root')!).render(
    <React.StrictMode><Root /></React.StrictMode>
  );
}

void mount();
