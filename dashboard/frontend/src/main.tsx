import { StrictMode, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { connect } from "./connection";
import { dismissSplashWhenReady } from "./splash";
import { Store } from "./store";
import { StoreContext } from "./useStore";
import "./styles.css";

// Module scope rather than component state: the splash is removed from outside
// React, and there is exactly one dashboard per page.
const store = new Store();

function Root() {
  useEffect(() => connect(store), []);
  return (
    <StoreContext.Provider value={store}>
      <App />
    </StoreContext.Provider>
  );
}

const container = document.getElementById("root");
if (!container) throw new Error("missing #root");

createRoot(container).render(
  <StrictMode>
    <Root />
  </StrictMode>,
);

dismissSplashWhenReady(store);
