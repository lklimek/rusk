import { get, writable } from "svelte/store";
import { browser } from "$app/environment";

/** @type {NetworkOption[]}*/
const networks = [
  { label: "Testnet", value: import.meta.env.VITE_DUSK_TESTNET_NODE },
  { label: "Devnet", value: import.meta.env.VITE_DUSK_DEVNET_NODE },
];

const maxWidthMediaQuery = window.matchMedia("(max-width: 1024px)");

const browserDefaults = browser
  ? {
      darkMode: window.matchMedia("(prefers-color-scheme: dark)").matches,
    }
  : {
      darkMode: false,
    };

/** @type {AppStoreContent} */
const initialState = {
  ...browserDefaults,
  blocksListEntries: Number(import.meta.env.VITE_BLOCKS_LIST_ENTRIES),
  chainInfoEntries: Number(import.meta.env.VITE_CHAIN_INFO_ENTRIES),
  fetchInterval: Number(import.meta.env.VITE_REFETCH_INTERVAL) || 1000,
  hasTouchSupport: "ontouchstart" in window || navigator.maxTouchPoints > 0,
  isSmallScreen: maxWidthMediaQuery.matches,
  marketDataFetchInterval:
    Number(import.meta.env.VITE_MARKET_DATA_REFETCH_INTERVAL) || 120000,
  network: networks[0].value,
  networks,
  statsFetchInterval:
    Number(import.meta.env.VITE_STATS_REFETCH_INTERVAL) || 1000,
  transactionsListEntries: Number(
    import.meta.env.VITE_TRANSACTIONS_LIST_ENTRIES
  ),
};

const store = writable(initialState);
const { set, subscribe } = store;

maxWidthMediaQuery.addEventListener("change", (event) => {
  set({
    ...get(store),
    isSmallScreen: event.matches,
  });
});

/** @param {string} network */
const setNetwork = (network) =>
  set({
    ...get(store),
    network,
  });

/** @param {boolean} darkMode */
const setTheme = (darkMode) => {
  set({
    ...get(store),
    darkMode,
  });
};

/** @type {AppStore} */
export default {
  setNetwork,
  setTheme,
  subscribe,
};
