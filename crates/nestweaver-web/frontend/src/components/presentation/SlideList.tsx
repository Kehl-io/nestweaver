import { useStore } from "../../stores";

export function SlideList() {
  const slides = useStore((s) => s.presentation.slides);
  const currentSlideIndex = useStore((s) => s.presentation.currentSlideIndex);
  const goToSlide = useStore((s) => s.goToSlide);
  const removeSlide = useStore((s) => s.removeSlide);

  if (slides.length === 0) {
    return (
      <div className="flex items-center justify-center h-full p-4">
        <p className="text-xs text-[var(--color-text-muted)] text-center">
          No slides yet. Build a presentation from the graph view.
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-1 p-2 overflow-y-auto">
      {slides.map((slide, index) => (
        <button
          key={slide.id}
          onClick={() => goToSlide(index)}
          className={`group relative flex items-start gap-2 rounded-md px-3 py-2 text-left text-xs transition-colors ${
            index === currentSlideIndex
              ? "bg-blue-600/10 text-blue-400 border border-blue-500/30"
              : "text-[var(--color-text-muted)] hover:bg-[var(--color-bg-secondary)] border border-transparent"
          }`}
        >
          <span className="font-mono font-semibold shrink-0">
            {index + 1}
          </span>
          <div className="min-w-0 flex-1">
            <span className="block font-medium capitalize">{slide.type}</span>
            {slide.annotation && (
              <span className="block truncate text-[10px] opacity-70 mt-0.5">
                {slide.annotation}
              </span>
            )}
          </div>
          <button
            onClick={(e) => {
              e.stopPropagation();
              removeSlide(slide.id);
            }}
            className="absolute right-2 top-2 hidden group-hover:block text-[var(--color-text-muted)] hover:text-red-400 transition-colors"
            aria-label="Delete slide"
          >
            <svg className="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
            </svg>
          </button>
        </button>
      ))}
    </div>
  );
}
