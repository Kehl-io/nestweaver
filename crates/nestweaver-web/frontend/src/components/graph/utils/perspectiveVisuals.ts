import type { GraphMode } from "../../../api/types";

interface PerspectiveVisuals {
  summary: string;
  sizeEncoding: string;
  colorEncoding: string;
}

export function perspectiveVisuals(mode: GraphMode): PerspectiveVisuals {
  switch (mode) {
    case "overview":
      return {
        summary: "Overview landmarks are bounded and ranked for first-read orientation.",
        sizeEncoding: "Size/glow: overview importance",
        colorEncoding: "Color: node kind",
      };
    case "context":
      return {
        summary: "Context mode emphasizes seeds and ranked related nodes.",
        sizeEncoding: "Size/glow: relevance and degree",
        colorEncoding: "Color: node kind",
      };
    case "local":
      return {
        summary: "Local mode keeps the selected item central while nearby nodes expand by depth.",
        sizeEncoding: "Size/glow: local degree and relevance",
        colorEncoding: "Color: node kind",
      };
    case "impact":
      return {
        summary: "Impact mode shows blast radius by dependency depth and confidence.",
        sizeEncoding: "Size: dependency degree",
        colorEncoding: "Color: depth and edge confidence",
      };
    case "repos":
      return {
        summary: "Architecture mode highlights repositories, services, hubs, and bridges.",
        sizeEncoding: "Size/glow: connectivity",
        colorEncoding: "Color: repository or service grouping",
      };
    case "features":
      return {
        summary: "Feature mode groups related implementation areas.",
        sizeEncoding: "Size/glow: cluster relevance",
        colorEncoding: "Color: feature grouping",
      };
    default:
      return {
        summary: "The active perspective controls graph emphasis.",
        sizeEncoding: "Size/glow: relevance",
        colorEncoding: "Color: node kind",
      };
  }
}
