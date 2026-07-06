import { useEffect, useCallback } from "react";
import { useHotkeys } from "react-hotkeys-hook";
import { useStore } from "../../stores";
import { SlideList } from "./SlideList";

export function PresentationView() {
  const presentation = useStore((s) => s.presentation);
  const setActiveView = useStore((s) => s.setActiveView);
  const nextSlide = useStore((s) => s.nextSlide);
  const prevSlide = useStore((s) => s.prevSlide);
  const togglePlayback = useStore((s) => s.togglePlayback);
  const modalOpen = useStore((s) => s.llmBarOpen || s.shortcutsOpen);

  const { slides, currentSlideIndex, isPlaying, name } = presentation;
  const currentSlide = slides[currentSlideIndex] ?? null;

  const goBack = useCallback(() => setActiveView("graph"), [setActiveView]);
  const presentationHotkeyOptions = { enabled: !modalOpen };

  useHotkeys("right", () => nextSlide(), presentationHotkeyOptions, [nextSlide]);
  useHotkeys("left", () => prevSlide(), presentationHotkeyOptions, [prevSlide]);
  useHotkeys("space", (e) => { e.preventDefault(); togglePlayback(); }, presentationHotkeyOptions, [togglePlayback]);
  useHotkeys("escape", goBack, presentationHotkeyOptions, [goBack]);

  // Auto-advance when playing
  useEffect(() => {
    if (!isPlaying || slides.length === 0) return;

    const durationMs = currentSlide?.durationMs ?? 3000;

    const timer = setTimeout(() => {
      if (currentSlideIndex < slides.length - 1) {
        nextSlide();
      } else {
        // Reached the end -- stop playback
        togglePlayback();
      }
    }, durationMs);

    return () => clearTimeout(timer);
  }, [isPlaying, currentSlideIndex, slides.length, currentSlide?.durationMs, nextSlide, togglePlayback]);

  return (
    <div className="flex h-full">
      {/* Slide list sidebar */}
      <div className="w-48 shrink-0 border-r border-[var(--color-border)] bg-[var(--color-bg-secondary)] overflow-y-auto">
        <div className="px-3 py-2 border-b border-[var(--color-border)]">
          <span className="text-[10px] font-semibold uppercase tracking-wider text-[var(--color-text-muted)]">
            Slides
          </span>
        </div>
        <SlideList />
      </div>

      {/* Main content */}
      <div className="flex flex-col flex-1 min-w-0">
        {/* Toolbar */}
        <div className="flex items-center gap-3 border-b border-[var(--color-border)] bg-[var(--color-bg-secondary)] px-4 py-2">
          <button
            onClick={goBack}
            className="flex items-center gap-1 text-sm text-[var(--color-text-muted)] hover:text-[var(--color-text)] transition-colors"
          >
            <span>&larr;</span>
            <span>Back</span>
          </button>

          <div className="h-4 w-px bg-[var(--color-border)]" />

          <span className="text-sm font-medium text-[var(--color-text)]">
            {name || "Untitled Presentation"}
          </span>

          <div className="flex-1" />

          <button
            onClick={prevSlide}
            disabled={slides.length === 0 || currentSlideIndex === 0}
            className="rounded border border-[var(--color-border)] px-2 py-1 text-xs text-[var(--color-text)] hover:bg-[var(--color-bg-secondary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
          >
            Prev
          </button>

          <span className="text-xs text-[var(--color-text-muted)] tabular-nums">
            {slides.length > 0
              ? `${currentSlideIndex + 1} / ${slides.length}`
              : "0 / 0"}
          </span>

          <button
            onClick={nextSlide}
            disabled={slides.length === 0 || currentSlideIndex >= slides.length - 1}
            className="rounded border border-[var(--color-border)] px-2 py-1 text-xs text-[var(--color-text)] hover:bg-[var(--color-bg-secondary)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
          >
            Next
          </button>

          <button
            onClick={togglePlayback}
            disabled={slides.length === 0}
            className="rounded bg-blue-600 px-3 py-1 text-xs font-medium text-white hover:bg-blue-700 disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
          >
            {isPlaying ? "Pause" : "Play"}
          </button>
        </div>

        {/* Slide content */}
        <div className="flex-1 flex items-center justify-center bg-[var(--color-bg)] p-8">
          {currentSlide ? (
            <div className="max-w-lg w-full rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-secondary)] p-8 shadow-sm text-center space-y-4">
              <span className="inline-block rounded-full bg-blue-600/10 px-3 py-1 text-xs font-semibold capitalize text-blue-400">
                {currentSlide.type}
              </span>

              {currentSlide.annotation && (
                <p className="text-sm text-[var(--color-text)]">
                  {currentSlide.annotation}
                </p>
              )}

              <p className="text-xs text-[var(--color-text-muted)]">
                {currentSlide.visibleNodes.length} visible node
                {currentSlide.visibleNodes.length !== 1 ? "s" : ""}
              </p>
            </div>
          ) : (
            <p className="text-sm text-[var(--color-text-muted)]">
              No slides. Build a presentation from the graph view.
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
