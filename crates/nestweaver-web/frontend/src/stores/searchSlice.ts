import type { StateCreator } from "zustand";
import type { SearchHit, SymbolCandidate } from "../api/types";
import type { PhraseIntent, PhraseResolution } from "../searchPhrases";
import type { StoreState } from "./index";

export interface SearchSlice {
  searchQuery: string;
  searchResults: SymbolCandidate[];
  brainSearchResults: SearchHit[];
  searchOpen: boolean;
  searchLoading: boolean;
  phraseIntent: PhraseIntent | null;
  phraseResolution: PhraseResolution | null;
  phraseResolving: boolean;
  phraseError: string | null;
  setSearchQuery: (q: string) => void;
  setSearchResults: (symbols: SymbolCandidate[], brain: SearchHit[]) => void;
  setSearchOpen: (open: boolean) => void;
  setSearchLoading: (loading: boolean) => void;
  setPhraseIntent: (intent: PhraseIntent | null) => void;
  setPhraseResolution: (resolution: PhraseResolution | null) => void;
  setPhraseResolving: (resolving: boolean) => void;
  setPhraseError: (error: string | null) => void;
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
  phraseIntent: null,
  phraseResolution: null,
  phraseResolving: false,
  phraseError: null,

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

  setPhraseIntent: (intent) =>
    set((s) => {
      s.phraseIntent = intent;
      s.phraseResolution = null;
      s.phraseError = null;
      s.phraseResolving = intent !== null;
    }),

  setPhraseResolution: (resolution) =>
    set((s) => {
      s.phraseResolution = resolution;
      s.phraseResolving = false;
      s.phraseError = null;
    }),

  setPhraseResolving: (resolving) =>
    set((s) => {
      s.phraseResolving = resolving;
    }),

  setPhraseError: (error) =>
    set((s) => {
      s.phraseError = error;
      s.phraseResolving = false;
    }),

  clearSearch: () =>
    set((s) => {
      s.searchQuery = "";
      s.searchResults = [];
      s.brainSearchResults = [];
      s.searchOpen = false;
      s.searchLoading = false;
      s.phraseIntent = null;
      s.phraseResolution = null;
      s.phraseResolving = false;
      s.phraseError = null;
    }),
});
