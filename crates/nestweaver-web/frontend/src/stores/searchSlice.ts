import type { StateCreator } from "zustand";
import type { SearchHit, SymbolCandidate } from "../api/types";
import type { StoreState } from "./index";

export interface SearchSlice {
  searchQuery: string;
  searchResults: SymbolCandidate[];
  brainSearchResults: SearchHit[];
  searchOpen: boolean;
  searchLoading: boolean;
  setSearchQuery: (q: string) => void;
  setSearchResults: (symbols: SymbolCandidate[], brain: SearchHit[]) => void;
  setSearchOpen: (open: boolean) => void;
  setSearchLoading: (loading: boolean) => void;
  clearSearch: () => void;
}

export const createSearchSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  SearchSlice
> = (set) => ({
  searchQuery: "",
  searchResults: [],
  brainSearchResults: [],
  searchOpen: false,
  searchLoading: false,

  setSearchQuery: (q) =>
    set((s) => {
      s.searchQuery = q;
    }),

  setSearchResults: (symbols, brain) =>
    set((s) => {
      s.searchResults = symbols;
      s.brainSearchResults = brain;
    }),

  setSearchOpen: (open) =>
    set((s) => {
      s.searchOpen = open;
    }),

  setSearchLoading: (loading) =>
    set((s) => {
      s.searchLoading = loading;
    }),

  clearSearch: () =>
    set((s) => {
      s.searchQuery = "";
      s.searchResults = [];
      s.brainSearchResults = [];
      s.searchOpen = false;
      s.searchLoading = false;
    }),
});
