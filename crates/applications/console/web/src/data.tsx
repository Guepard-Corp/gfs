import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from 'react';

import { api, type GfsDatabase, type Telemetry } from './api';

const POLL_MS = 4000;

type DataState = {
  databases: GfsDatabase[];
  telemetry: Telemetry | null;
  error: string | null;
  refresh: () => Promise<void>;
};

const DataContext = createContext<DataState | null>(null);

export function DataProvider({ children }: { children: ReactNode }) {
  const [databases, setDatabases] = useState<GfsDatabase[]>([]);
  const [telemetry, setTelemetry] = useState<Telemetry | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [dbs, tel] = await Promise.all([api.databases(), api.telemetry()]);
      setDatabases(dbs);
      setTelemetry(tel);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const id = setInterval(() => void refresh(), POLL_MS);
    return () => clearInterval(id);
  }, [refresh]);

  return (
    <DataContext.Provider value={{ databases, telemetry, error, refresh }}>
      {children}
    </DataContext.Provider>
  );
}

export function useData(): DataState {
  const ctx = useContext(DataContext);
  if (!ctx) throw new Error('useData must be used within DataProvider');
  return ctx;
}
