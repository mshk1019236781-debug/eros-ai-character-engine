import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import './index.css';
import { RootApp } from './RootApp.tsx';
import { RenderFixture } from './devtools/RenderFixture.tsx';
import { checkAndImportSeed } from './lib/seedManager.ts';

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

// Dev-only: `?phase2_fixture=1` swaps the app for the labelled MOCK render
// screen. The normal entry never takes this branch.
const renderFixtureRequested =
  !isTauri && new URLSearchParams(window.location.search).has('phase2_fixture');

if (isTauri) document.documentElement.classList.add('tauri-mode');

async function init() {
  if (isTauri) {
    // On fresh install: auto-import seed data if present next to exe
    await checkAndImportSeed();
  }

  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      {renderFixtureRequested
        ? <RenderFixture />
        : <RootApp />}
    </StrictMode>,
  );
}

init();
