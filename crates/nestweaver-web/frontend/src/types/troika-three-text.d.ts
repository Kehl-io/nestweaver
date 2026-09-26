// troika-three-text ships no type declarations. Only the typesetting config
// entry point is imported directly (nw-625); drei wraps everything else.
declare module "troika-three-text" {
  export interface TextBuilderConfig {
    defaultFontURL?: string | null;
    unicodeFontsURL?: string | null;
    sdfGlyphSize?: number;
    sdfMargin?: number;
    sdfExponent?: number;
    textureWidth?: number;
    /** Typeset in a web worker (default true). */
    useWorker?: boolean;
  }
  export function configureTextBuilder(config: TextBuilderConfig): void;
}
