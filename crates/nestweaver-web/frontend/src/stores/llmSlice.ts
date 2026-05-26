import type { StateCreator } from "zustand";
import type { StoreState } from "./index";
import type { BrainContextResult } from "../api/types";

export interface LlmQueryResult {
  seeds: string[];
  explanation: string;
  context: BrainContextResult;
}

export interface LlmSlice {
  llmBarOpen: boolean;
  llmQuery: string;
  llmLoading: boolean;
  llmResult: LlmQueryResult | null;
  llmError: string | null;
  openLlmBar: () => void;
  closeLlmBar: () => void;
  setLlmQuery: (query: string) => void;
  setLlmLoading: (loading: boolean) => void;
  setLlmResult: (result: LlmQueryResult | null) => void;
  setLlmError: (error: string | null) => void;
  clearLlm: () => void;
}

export const createLlmSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  LlmSlice
> = (set) => ({
  llmBarOpen: false,
  llmQuery: "",
  llmLoading: false,
  llmResult: null,
  llmError: null,
  openLlmBar: () =>
    set((s) => {
      s.llmBarOpen = true;
    }),
  closeLlmBar: () =>
    set((s) => {
      s.llmBarOpen = false;
      s.llmQuery = "";
      s.llmError = null;
    }),
  setLlmQuery: (query) =>
    set((s) => {
      s.llmQuery = query;
    }),
  setLlmLoading: (loading) =>
    set((s) => {
      s.llmLoading = loading;
    }),
  setLlmResult: (result) =>
    set((s) => {
      s.llmResult = result;
    }),
  setLlmError: (error) =>
    set((s) => {
      s.llmError = error;
    }),
  clearLlm: () =>
    set((s) => {
      s.llmBarOpen = false;
      s.llmQuery = "";
      s.llmLoading = false;
      s.llmResult = null;
      s.llmError = null;
    }),
});
