import type { StateCreator } from "zustand";
import type { Perspective } from "../api/types";
import type { StoreState } from "./index";

export type ContentView = "graph" | "canvas" | "presentation";

export interface CanvasElement {
  id: string;
  type: "symbol" | "note" | "text" | "image";
  uid?: string;
  content?: string;
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface CanvasConnection {
  id: string;
  fromId: string;
  toId: string;
  label?: string;
}

export interface CanvasSection {
  id: string;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  color: string;
}

export interface CanvasState {
  id: string;
  name: string;
  elements: CanvasElement[];
  connections: CanvasConnection[];
  sections: CanvasSection[];
  selectedElementId: string | null;
  draggedElementId: string | null;
}

export interface Slide {
  id: string;
  type: "reveal" | "focus" | "annotate" | "transition";
  visibleNodes: string[];
  camera: { x: number; y: number; ratio: number };
  annotation?: string;
  durationMs: number;
}

export interface PresentationState {
  id: string;
  name: string;
  slides: Slide[];
  currentSlideIndex: number;
  isPlaying: boolean;
  isBuilding: boolean;
}

export interface TimelineEntry {
  sha: string;
  timestamp: number;
  message: string;
  symbols_added: number;
  symbols_removed: number;
  symbols_modified: number;
}

export interface ContentSlice {
  activeView: ContentView;
  setActiveView: (view: ContentView) => void;

  perspectives: Perspective[];
  activePerspectiveId: string | null;
  setPerspectives: (perspectives: Perspective[]) => void;
  setActivePerspectiveId: (id: string | null) => void;

  canvas: CanvasState;
  setCanvasId: (id: string) => void;
  addElement: (element: CanvasElement) => void;
  updateElement: (id: string, patch: Partial<CanvasElement>) => void;
  removeElement: (id: string) => void;
  addConnection: (connection: CanvasConnection) => void;
  removeConnection: (id: string) => void;
  addSection: (section: CanvasSection) => void;
  selectElement: (id: string | null) => void;
  setDragged: (id: string | null) => void;
  clearCanvas: () => void;

  presentation: PresentationState;
  setPresentationId: (id: string) => void;
  addSlide: (slide: Slide) => void;
  removeSlide: (id: string) => void;
  goToSlide: (index: number) => void;
  nextSlide: () => void;
  prevSlide: () => void;
  togglePlayback: () => void;
  setBuilding: (building: boolean) => void;

  timelineEntries: TimelineEntry[];
  timelinePosition: number;
  timelinePlaying: boolean;
  setTimelineEntries: (entries: TimelineEntry[]) => void;
  setTimelinePosition: (position: number) => void;
  setTimelinePlaying: (playing: boolean) => void;

  sseConnected: boolean;
  lastEventTimestamp: number | null;
  setSseConnected: (connected: boolean) => void;
  setLastEventTimestamp: (timestamp: number | null) => void;

  theme: "system" | "light" | "dark";
  setTheme: (theme: "system" | "light" | "dark") => void;
}

const emptyCanvas: CanvasState = {
  id: "",
  name: "",
  elements: [],
  connections: [],
  sections: [],
  selectedElementId: null,
  draggedElementId: null,
};

const emptyPresentation: PresentationState = {
  id: "",
  name: "",
  slides: [],
  currentSlideIndex: 0,
  isPlaying: false,
  isBuilding: false,
};

export const createContentSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  ContentSlice
> = (set) => ({
  activeView: "graph",
  setActiveView: (view) =>
    set((s) => {
      s.activeView = view;
    }),

  perspectives: [],
  activePerspectiveId: null,
  setPerspectives: (perspectives) =>
    set((s) => {
      s.perspectives = perspectives;
    }),
  setActivePerspectiveId: (id) =>
    set((s) => {
      s.activePerspectiveId = id;
    }),

  canvas: { ...emptyCanvas },
  setCanvasId: (id) =>
    set((s) => {
      s.canvas.id = id;
    }),
  addElement: (element) =>
    set((s) => {
      s.canvas.elements.push(element);
    }),
  updateElement: (id, patch) =>
    set((s) => {
      const el = s.canvas.elements.find((e) => e.id === id);
      if (el) Object.assign(el, patch);
    }),
  removeElement: (id) =>
    set((s) => {
      s.canvas.elements = s.canvas.elements.filter((e) => e.id !== id);
      if (s.canvas.selectedElementId === id) s.canvas.selectedElementId = null;
      if (s.canvas.draggedElementId === id) s.canvas.draggedElementId = null;
    }),
  addConnection: (connection) =>
    set((s) => {
      s.canvas.connections.push(connection);
    }),
  removeConnection: (id) =>
    set((s) => {
      s.canvas.connections = s.canvas.connections.filter((c) => c.id !== id);
    }),
  addSection: (section) =>
    set((s) => {
      s.canvas.sections.push(section);
    }),
  selectElement: (id) =>
    set((s) => {
      s.canvas.selectedElementId = id;
    }),
  setDragged: (id) =>
    set((s) => {
      s.canvas.draggedElementId = id;
    }),
  clearCanvas: () =>
    set((s) => {
      s.canvas = { ...emptyCanvas };
    }),

  presentation: { ...emptyPresentation },
  setPresentationId: (id) =>
    set((s) => {
      s.presentation.id = id;
    }),
  addSlide: (slide) =>
    set((s) => {
      s.presentation.slides.push(slide);
    }),
  removeSlide: (id) =>
    set((s) => {
      const idx = s.presentation.slides.findIndex((sl) => sl.id === id);
      if (idx >= 0) {
        s.presentation.slides.splice(idx, 1);
        if (s.presentation.currentSlideIndex >= s.presentation.slides.length) {
          s.presentation.currentSlideIndex = Math.max(
            0,
            s.presentation.slides.length - 1,
          );
        }
      }
    }),
  goToSlide: (index) =>
    set((s) => {
      s.presentation.currentSlideIndex = index;
    }),
  nextSlide: () =>
    set((s) => {
      if (
        s.presentation.currentSlideIndex <
        s.presentation.slides.length - 1
      ) {
        s.presentation.currentSlideIndex += 1;
      }
    }),
  prevSlide: () =>
    set((s) => {
      if (s.presentation.currentSlideIndex > 0) {
        s.presentation.currentSlideIndex -= 1;
      }
    }),
  togglePlayback: () =>
    set((s) => {
      s.presentation.isPlaying = !s.presentation.isPlaying;
    }),
  setBuilding: (building) =>
    set((s) => {
      s.presentation.isBuilding = building;
    }),

  timelineEntries: [],
  timelinePosition: 0,
  timelinePlaying: false,
  setTimelineEntries: (entries) =>
    set((s) => {
      s.timelineEntries = entries;
    }),
  setTimelinePosition: (position) =>
    set((s) => {
      s.timelinePosition = position;
    }),
  setTimelinePlaying: (playing) =>
    set((s) => {
      s.timelinePlaying = playing;
    }),

  sseConnected: false,
  lastEventTimestamp: null,
  setSseConnected: (connected) =>
    set((s) => {
      s.sseConnected = connected;
    }),
  setLastEventTimestamp: (timestamp) =>
    set((s) => {
      s.lastEventTimestamp = timestamp;
    }),

  theme: "system",
  setTheme: (theme) =>
    set((s) => {
      s.theme = theme;
    }),
});
