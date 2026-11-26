import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { Root } from '@/root';

const root = document.getElementById('root');
if (!root) throw new Error('index.html has no #root');

createRoot(root).render(
  <StrictMode>
    <Root />
  </StrictMode>,
);
