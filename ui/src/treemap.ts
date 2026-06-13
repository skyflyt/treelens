/**
 * Canvas treemap renderer.
 *
 * Receives a flat `Rect[]` (computed in Rust) and renders them with hover
 * highlight, click selection, double-click drill-in, and an optional age-heat
 * coloring mode. Pan/zoom of the static layout is intentionally omitted — the
 * UX is "click to drill into a directory" + animated transition.
 */
import { type Rect } from "./ipc";
import { type ColorMode, colorForRect } from "./colors";

export interface TreemapTheme {
  dark: boolean;
  borderColor: string;
  textColor: string;
  bgColor: string;
}

export interface TreemapInteractions {
  onHover(rect: Rect | null, x: number, y: number): void;
  onClick(rect: Rect): void;
  onDoubleClick(rect: Rect): void;
  onContextMenu(rect: Rect, x: number, y: number): void;
}

export interface RectNameLookup {
  (idx: number): string;
}

export class Treemap {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private rects: Rect[] = [];
  private rootIdx = 0;
  private theme: TreemapTheme;
  private mode: ColorMode = "type";
  private hoverIdx: number | null = null;
  private selectedIdx: number | null = null;
  private nameOf: RectNameLookup = () => "";
  private interactions: TreemapInteractions;
  private dpr = 1;
  private widthCss = 0;
  private heightCss = 0;

  constructor(canvas: HTMLCanvasElement, theme: TreemapTheme, interactions: TreemapInteractions) {
    this.canvas = canvas;
    this.theme = theme;
    this.interactions = interactions;
    const ctx = canvas.getContext("2d", { alpha: false });
    if (!ctx) throw new Error("2D context unavailable");
    this.ctx = ctx;
    canvas.addEventListener("mousemove", this.handleMouseMove);
    canvas.addEventListener("mouseleave", this.handleMouseLeave);
    canvas.addEventListener("click", this.handleClick);
    canvas.addEventListener("dblclick", this.handleDblClick);
    canvas.addEventListener("contextmenu", this.handleContext);
  }

  setTheme(theme: TreemapTheme) {
    this.theme = theme;
    this.draw();
  }
  setMode(mode: ColorMode) {
    this.mode = mode;
    this.draw();
  }
  setSelected(idx: number | null) {
    this.selectedIdx = idx;
    this.draw();
  }
  setData(rects: Rect[], rootIdx: number, nameOf: RectNameLookup) {
    this.rects = rects;
    this.rootIdx = rootIdx;
    this.nameOf = nameOf;
    this.hoverIdx = null;
    this.draw();
  }

  /** Resize canvas backing store to match its CSS size and the device pixel ratio. */
  resize(cssWidth: number, cssHeight: number) {
    this.dpr = Math.max(1, Math.min(2.5, window.devicePixelRatio || 1));
    this.widthCss = cssWidth;
    this.heightCss = cssHeight;
    this.canvas.width = Math.floor(cssWidth * this.dpr);
    this.canvas.height = Math.floor(cssHeight * this.dpr);
    this.canvas.style.width = cssWidth + "px";
    this.canvas.style.height = cssHeight + "px";
    this.draw();
  }

  hitTest(x: number, y: number): Rect | null {
    // Walk from end to start: the layout pushes deeper rects later, and we want
    // the smallest (deepest) rect at this point.
    for (let i = this.rects.length - 1; i >= 0; i--) {
      const r = this.rects[i];
      if (r.idx === this.rootIdx) continue;
      if (x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h) {
        return r;
      }
    }
    return null;
  }

  draw() {
    const ctx = this.ctx;
    ctx.save();
    ctx.scale(this.dpr, this.dpr);
    ctx.fillStyle = this.theme.bgColor;
    ctx.fillRect(0, 0, this.widthCss, this.heightCss);

    // Two passes: deeper rects on top, but we render in-order since layout is
    // already child-after-parent.
    for (const r of this.rects) {
      if (r.idx === this.rootIdx) continue;
      const name = r.name || this.nameOf(r.idx);
      const base = colorForRect(r, name, this.mode, { dark: this.theme.dark });
      ctx.fillStyle = base;
      ctx.fillRect(r.x, r.y, r.w, r.h);

      // Faint cushion-style inner shadow for the classic treemap depth cue, but cheap:
      // a single linear gradient across the rect rather than per-pixel cushion math.
      if (!r.is_dir && r.w > 4 && r.h > 4) {
        const g = ctx.createLinearGradient(r.x, r.y, r.x, r.y + r.h);
        const tint = this.theme.dark ? "rgba(255,255,255,0.06)" : "rgba(255,255,255,0.18)";
        const shade = this.theme.dark ? "rgba(0,0,0,0.18)" : "rgba(0,0,0,0.10)";
        g.addColorStop(0, tint);
        g.addColorStop(1, shade);
        ctx.fillStyle = g;
        ctx.fillRect(r.x, r.y, r.w, r.h);
      }

      // Border
      ctx.lineWidth = r.is_dir ? 1 : 0.5;
      ctx.strokeStyle = this.theme.borderColor;
      ctx.strokeRect(r.x + 0.5, r.y + 0.5, r.w - 1, r.h - 1);

      // Label: directory header for dirs with room, or a centered file label for big files.
      if (name) {
        if (r.is_dir && r.w > 50 && r.h > 18) {
          const padX = 4;
          const padY = 3;
          ctx.fillStyle = this.theme.dark ? "rgba(0,0,0,0.35)" : "rgba(255,255,255,0.55)";
          ctx.fillRect(r.x, r.y, r.w, 16);
          ctx.fillStyle = this.theme.dark ? "#e5e7eb" : "#1f2937";
          ctx.font = "600 11px ui-sans-serif, system-ui";
          ctx.textBaseline = "top";
          const text = clip(ctx, name, r.w - padX * 2);
          ctx.fillText(text, r.x + padX, r.y + padY);
        } else if (!r.is_dir && r.w > 60 && r.h > 22) {
          ctx.fillStyle = this.theme.dark ? "rgba(255,255,255,0.85)" : "rgba(255,255,255,0.95)";
          ctx.font = "500 10.5px ui-sans-serif, system-ui";
          ctx.textBaseline = "middle";
          const text = clip(ctx, name, r.w - 8);
          ctx.fillText(text, r.x + 4, r.y + r.h / 2);
        }
      }
    }

    // Hover/selection overlay.
    if (this.hoverIdx !== null) {
      const r = this.rects.find((rr) => rr.idx === this.hoverIdx);
      if (r) {
        ctx.fillStyle = this.theme.dark ? "rgba(255,255,255,0.12)" : "rgba(0,0,0,0.07)";
        ctx.fillRect(r.x, r.y, r.w, r.h);
        ctx.lineWidth = 2;
        ctx.strokeStyle = this.theme.dark ? "#93c5fd" : "#1d4ed8";
        ctx.strokeRect(r.x + 1, r.y + 1, r.w - 2, r.h - 2);
      }
    }
    if (this.selectedIdx !== null && this.selectedIdx !== this.hoverIdx) {
      const r = this.rects.find((rr) => rr.idx === this.selectedIdx);
      if (r) {
        ctx.lineWidth = 2;
        ctx.strokeStyle = this.theme.dark ? "#fbbf24" : "#b45309";
        ctx.strokeRect(r.x + 1, r.y + 1, r.w - 2, r.h - 2);
      }
    }
    ctx.restore();
  }

  private handleMouseMove = (e: MouseEvent) => {
    const { left, top } = this.canvas.getBoundingClientRect();
    const x = e.clientX - left;
    const y = e.clientY - top;
    const r = this.hitTest(x, y);
    const newIdx = r?.idx ?? null;
    if (newIdx !== this.hoverIdx) {
      this.hoverIdx = newIdx;
      this.draw();
    }
    this.interactions.onHover(r, e.clientX, e.clientY);
  };

  private handleMouseLeave = () => {
    if (this.hoverIdx !== null) {
      this.hoverIdx = null;
      this.draw();
    }
    this.interactions.onHover(null, 0, 0);
  };

  private handleClick = (e: MouseEvent) => {
    const { left, top } = this.canvas.getBoundingClientRect();
    const r = this.hitTest(e.clientX - left, e.clientY - top);
    if (r) this.interactions.onClick(r);
  };

  private handleDblClick = (e: MouseEvent) => {
    const { left, top } = this.canvas.getBoundingClientRect();
    const r = this.hitTest(e.clientX - left, e.clientY - top);
    if (r) this.interactions.onDoubleClick(r);
  };

  private handleContext = (e: MouseEvent) => {
    e.preventDefault();
    const { left, top } = this.canvas.getBoundingClientRect();
    const r = this.hitTest(e.clientX - left, e.clientY - top);
    if (r) this.interactions.onContextMenu(r, e.clientX, e.clientY);
  };
}

function clip(ctx: CanvasRenderingContext2D, text: string, maxW: number): string {
  if (ctx.measureText(text).width <= maxW) return text;
  const ell = "…";
  let lo = 0;
  let hi = text.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    const slice = text.slice(0, mid) + ell;
    if (ctx.measureText(slice).width <= maxW) lo = mid;
    else hi = mid - 1;
  }
  return lo > 0 ? text.slice(0, lo) + ell : "";
}
