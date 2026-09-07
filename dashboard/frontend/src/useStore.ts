import { createContext, useContext, useSyncExternalStore } from "react";
import type { Store } from "./store";

export const StoreContext = createContext<Store | null>(null);

function useStoreInstance(): Store {
  const store = useContext(StoreContext);
  if (!store) throw new Error("StoreContext is missing a provider");
  return store;
}

/**
 * Re-renders the caller whenever anything in the store changes.
 *
 * This is coarse on purpose. The store already coalesces updates to one per
 * frame, and the dashboard is small enough that per-key subscriptions would
 * cost more in complexity than they would save in renders.
 */
export function useStore(): Store {
  const store = useStoreInstance();
  useSyncExternalStore(store.subscribe, store.getRevision, store.getRevision);
  return store;
}
