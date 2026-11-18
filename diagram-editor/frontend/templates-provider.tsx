import {
  createContext,
  type Dispatch,
  type PropsWithChildren,
  type SetStateAction,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
} from 'react';
import type { SectionTemplate } from './types/api';

export type TemplatesContext = [
  Record<string, SectionTemplate>,
  Dispatch<SetStateAction<Record<string, SectionTemplate>>>,
];

const TemplatesContextComp = createContext<TemplatesContext | null>(null);

export function TemplatesProvider({ children }: PropsWithChildren) {
  const serverTemplatesRef = useRef<Record<string, SectionTemplate>>({});
  const [templates, _setTemplates] = useState<
    Record<string, SectionTemplate>
  >({});

  // Wrap setTemplates so server-side templates always appear as the base
  // layer. Diagram-local templates (spread after) take precedence.
  const setTemplates: Dispatch<
    SetStateAction<Record<string, SectionTemplate>>
  > = useCallback((action) => {
    _setTemplates((prev) => {
      const next = typeof action === 'function' ? action(prev) : action;
      return { ...serverTemplatesRef.current, ...next };
    });
  }, []);

  useEffect(() => {
    fetch('/api/templates')
      .then((res) => (res.ok ? res.json() : {}))
      .then((server: Record<string, SectionTemplate>) => {
        serverTemplatesRef.current = server;
        _setTemplates((prev) => ({ ...server, ...prev }));
      })
      .catch(() => {});
  }, []);

  return (
    <TemplatesContextComp.Provider value={[templates, setTemplates]}>
      {children}
    </TemplatesContextComp.Provider>
  );
}

export function useTemplates(): TemplatesContext {
  const context = useContext(TemplatesContextComp);
  if (!context) {
    throw new Error('useTemplates must be used within a TemplatesProvider');
  }
  return context;
}
